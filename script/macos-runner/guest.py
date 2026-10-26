#!/usr/bin/env python3
"""Trusted, root-only guest provisioning, measurement and Remote Login sealing."""

import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile

import parity
import pinlock

PREFIX = Path("/opt/motor")
HERE = Path(__file__).resolve().parent


def run(*args, env=None, cwd=None):
    subprocess.run([str(value) for value in args], check=True, env=env, cwd=cwd)


def unpack(archive, destination):
    destination.mkdir(parents=True, exist_ok=False)
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            for name in source.namelist():
                if Path(name).is_absolute() or ".." in Path(name).parts:
                    raise ValueError("archive path escapes extraction root")
            source.extractall(destination)
    else:
        with tarfile.open(archive) as source:
            # Even a hash-checked archive must never write outside scratch.
            for member in source.getmembers():
                if Path(member.name).is_absolute() or ".." in Path(member.name).parts or member.isdev() or member.isfifo():
                    raise ValueError("unsafe source archive entry")
                if member.issym() or member.islnk():
                    if Path(member.linkname).is_absolute() or ".." in Path(member.linkname).parts:
                        raise ValueError("unsafe source archive link")
            source.extractall(destination)
    return destination


def account():
    import pwd
    try:
        job = pwd.getpwnam("motorjob")
    except KeyError:
        used = {entry.pw_uid for entry in pwd.getpwall()}
        uid = next(value for value in range(501, 1000) if value not in used)
        run("/usr/bin/dscl", ".", "-create", "/Users/motorjob")
        for key, value in {"UniqueID": str(uid), "PrimaryGroupID": "20", "NFSHomeDirectory": "/Users/motorjob",
                           "UserShell": "/usr/bin/false", "Password": "*", "RealName": "Motor guest job"}.items():
            run("/usr/bin/dscl", ".", "-create", "/Users/motorjob", key, value)
        job = pwd.getpwnam("motorjob")
    groups = subprocess.check_output(["/usr/bin/id", "-Gn", "motorjob"], text=True).split()
    if job.pw_uid == 0 or job.pw_gid == 0 or "admin" in groups:
        raise ValueError("motorjob must be non-root and non-admin")
    Path(job.pw_dir).mkdir(exist_ok=True)
    os.chown(job.pw_dir, job.pw_uid, job.pw_gid)


def clt_verify():
    # softwareupdate authenticates the package; check the installed Apple code too.
    run("/usr/bin/codesign", "--verify", "--strict", "-R", "=anchor apple", "/Library/Developer/CommandLineTools/usr/bin/clang")
    receipt = subprocess.check_output(["/usr/sbin/pkgutil", "--pkg-info-plist", "com.apple.pkg.CLTools_Executables"])
    return plistlib.loads(receipt)["pkg-version"]


def provision(manifest, scratch, measure):
    if not measure:
        pinlock.require_ready(manifest)
    clt = clt_verify()
    if not measure and clt != manifest["clt"]["version"]:
        raise ValueError("Command Line Tools version differs")
    account()
    PREFIX.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, HOME="/var/root", RUSTUP_HOME=manifest["rustup_home"], CARGO_HOME=manifest["cargo_home"],
               RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", CARGO_BUILD_JOBS="2")
    env["PATH"] = manifest["job_environment_by_role"]["macos"]["PATH"]
    archives = scratch / "inputs"
    sources = {}
    for name, spec in manifest["sources"].items():
        path = archives / name
        value = parity.digest(path)
        if parity.sha256(spec["sha256"]) and value != spec["sha256"]:
            raise ValueError("source SHA-256 differs: " + name)
        sources[name] = value
    # rustup picks its mode from the name it runs under: only as rustup-init
    # does it install. The download is stored under its source name.
    init = archives / "rustup-init"
    shutil.copyfile(archives / "rustup", init)
    init.chmod(0o755)
    run(init, "-y", "--no-modify-path", "--profile", manifest["rust_profile"], "--default-toolchain", "none", env=env)
    rustup = PREFIX / "cargo/bin/rustup"
    for name, spec in manifest["rust_toolchains"].items():
        if name.startswith("stable-"):
            continue
        components = [value.removesuffix("-" + parity.HOST) for value in spec["components"]
                      if value != "rust-std-" + parity.HOST]
        run(rustup, "toolchain", "install", name, "--profile", "minimal", "--component", ",".join(components), env=env)
    stable = PREFIX / "rustup/toolchains" / ("stable-" + parity.HOST)
    if stable.exists():
        shutil.rmtree(stable)
    shutil.copytree(PREFIX / "rustup/toolchains" / manifest["stable_copy_from"], stable, symlinks=True)
    source_components = manifest["rust_toolchains"][manifest["stable_copy_from"]]["components"]
    stable_components = manifest["rust_toolchains"]["stable-" + parity.HOST]["components"]
    for component in sorted(set(source_components) - set(stable_components)):
        run(rustup, "component", "remove", "--toolchain", "stable-" + parity.HOST, component, env=env)
    run(rustup, "default", manifest["rust_default"], env=env)
    for name in ("bash", "git"):
        archive = scratch / (name + ".tar")
        shutil.copyfile(archives / name, archive)
        root = unpack(archive, scratch / (name + "-source"))
        root = next(path for path in root.iterdir() if path.is_dir())
        run("/bin/sh", root / "configure", "--prefix=" + str(PREFIX), cwd=root, env=env)
        flags = ["NO_GETTEXT=YesPlease", "NO_TCLTK=YesPlease", "RUNTIME_PREFIX=YesPlease"] if name == "git" else []
        run("/usr/bin/make", "-j2", *flags, cwd=root, env=env)
        run("/usr/bin/make", "install", *flags, cwd=root, env=env)
    bun_archive = scratch / "bun.zip"
    shutil.copyfile(archives / "bun", bun_archive)
    bun_root = unpack(bun_archive, scratch / "bun-source")
    (PREFIX / "bun/bin").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(bun_root / "bun-darwin-aarch64/bun", PREFIX / "bun/bin/bun")
    (PREFIX / "bun/bin/bun").chmod(0o755)
    cargo = PREFIX / "cargo/bin/cargo"
    for index, tool in enumerate(manifest["cargo_installs"]):
        root = scratch / ("cargo-install-" + str(index))
        command = [cargo, "install", "--locked", "--root", root]
        if "git" in tool:
            path = archives / ("cargo-" + tool["rev"])
            value = parity.digest(path)
            if parity.sha256(tool["source"]["sha256"]) and value != tool["source"]["sha256"]:
                raise ValueError("Cargo source SHA-256 differs")
            tool["source"]["sha256"] = value
            source_root = unpack(path, scratch / ("cargo-source-" + str(index)))
            packages = []
            for toml in source_root.rglob("Cargo.toml"):
                package = re.search(r"(?ms)^\[package\]\s*(.*?)(?=^\[|\Z)", toml.read_text())
                if package and re.search(r'^name\s*=\s*"' + re.escape(tool["crate"]) + '"', package.group(1), re.M):
                    packages.append(toml.parent)
            if len(packages) != 1:
                raise ValueError("cannot uniquely locate pinned Cargo package " + tool["crate"])
            command += ["--path", packages[0]]
        else:
            command += [tool["crate"], "--version", "=" + tool["version"]]
        run(*command, env=env)
        for name in tool["bins"]:
            variant = name + "-" + tool["rev"][:8] if "git" in tool else name
            shutil.copy2(root / "bin" / name, PREFIX / "cargo/bin" / variant)
            if tool.get("default"):
                shutil.copy2(root / "bin" / name, PREFIX / "cargo/bin" / name)
    # Use stable relative proxies rather than hardlinks to rustup's executable.
    for name, spec in manifest["cargo_binaries"].items():
        if spec.get("proxy"):
            path = PREFIX / "cargo/bin" / name
            path.unlink(missing_ok=True)
            path.symlink_to("rustup")
    source = unpack(archives / "agent.tar", scratch / "agent-source")
    if (archives / "agent.commit").read_text().strip() != manifest["agent"]["commit"]:
        raise ValueError("agent source archive commit differs")
    run(cargo, "build", "--release", "--locked", "-p", "motor-macagent", "--target", parity.HOST, cwd=source, env=env)
    agent = PREFIX / "bin/motor-macagent"
    shutil.copy2(source / "target" / parity.HOST / "release/motor-macagent", agent)
    agent_sha = parity.digest(agent)
    if parity.sha256(manifest["agent"]["sha256"]) and manifest["agent"]["sha256"] != agent_sha:
        raise ValueError("agent binary SHA-256 differs")
    plist = {"Label": "io.cortexkit.motor.macagent", "ProgramArguments": [str(agent)], "UserName": "root",
             "RunAtLoad": True, "KeepAlive": True, "Umask": 0o077}
    agent_plist = Path("/Library/LaunchDaemons/io.cortexkit.motor.macagent.plist")
    agent_plist.write_bytes(plistlib.dumps(plist))
    agent_plist.chmod(0o644)
    (PREFIX / "agent.sha256").write_text(agent_sha + "\n")
    tools = scratch / "tools"
    tools.mkdir()
    shutil.copytree(PREFIX / "rustup/toolchains" / manifest["rust_default"] / "lib", tools / "lib", symlinks=True)
    (tools / "bin").mkdir()
    for name in ("cargo", "rustc"):
        shutil.copy2(PREFIX / "rustup/toolchains" / manifest["rust_default"] / "bin" / name, tools / "bin" / name)
    for name in ("bun", "git"):
        shutil.copy2(Path(manifest["tools"][name]["path"]), tools / "bin" / name)
    shutil.copytree(PREFIX / "libexec", tools / "libexec", symlinks=True)
    (tools / "share").mkdir()
    shutil.copytree(PREFIX / "share/git-core", tools / "share/git-core", symlinks=True)
    tool_sha = {name: parity.digest(tools / spec["path"]) for name, spec in manifest["prep_tools"].items()}
    (PREFIX / "toolchain.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for path in [PREFIX, *PREFIX.rglob("*")]:
        if not path.is_symlink():
            os.chown(path, 0, 0)
            path.chmod(0o555 if path.is_dir() or os.access(path, os.X_OK) else 0o444)
    actual = parity.capture(manifest)
    result = pinlock.candidate(manifest, actual, sources, agent_sha, tool_sha)
    (scratch / "candidate.json").write_text(json.dumps(result, indent=2) + "\n")
    if not measure:
        errors = parity.differences(manifest, actual)
        if errors or tool_sha != {name: row["sha256"] for name, row in manifest["prep_tools"].items()}:
            raise ValueError("built tools differ from reviewed pins: " + "; ".join(errors))
    print("Candidate measured. Review and commit its pins before sealing.")


def drop_build_access(build_user, home, sudoers=Path("/etc/sudoers.d/motorbuild")):
    run("/usr/sbin/systemsetup", "-f", "-setremotelogin", "off")
    status = subprocess.check_output(["/usr/sbin/systemsetup", "-getremotelogin"], text=True)
    if "Off" not in status:
        raise ValueError("Remote Login did not turn off")
    if (home / ".ssh").is_symlink():
        raise ValueError("build account .ssh is a symlink")
    if (home / ".ssh").exists():
        shutil.rmtree(home / ".ssh")
    run("/usr/bin/dscl", ".", "-create", "/Users/" + build_user, "UserShell", "/usr/bin/false")
    run("/usr/bin/dscl", ".", "-create", "/Users/" + build_user, "Password", "*")
    sudoers.unlink(missing_ok=True)


def seal(manifest, build_user, scratch):
    import pwd
    pinlock.require_ready(manifest)
    manifest_sha = parity.digest(HERE / "toolchain.json")
    clt_verify()
    (PREFIX / "toolchain.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (PREFIX / "toolchain.json").chmod(0o444)
    errors = parity.differences(manifest, parity.capture(manifest))
    if errors or parity.digest(PREFIX / "bin/motor-macagent") != manifest["agent"]["sha256"]:
        raise ValueError("guest tools differ from reviewed pins: " + "; ".join(errors))
    home = Path(pwd.getpwnam(build_user).pw_dir)
    if build_user in ("root", "motorjob") or not home.is_relative_to(Path("/Users")):
        raise ValueError("invalid build-only account")
    drop_build_access(build_user, home)
    # Build sources and the privileged bootstrap key are not part of a job image.
    shutil.rmtree(scratch)
    (PREFIX / "sealed.json").write_text(json.dumps({"manifest_sha256": manifest_sha,
                                                   "agent_sha256": manifest["agent"]["sha256"],
                                                   "remote_login": "Off"}) + "\n")
    print("MOTOR_SEALED " + manifest_sha, flush=True)
    run("/sbin/shutdown", "-h", "now")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("provision", "seal"))
    parser.add_argument("--measure", action="store_true")
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--build-user", default="motorbuild")
    args = parser.parse_args()
    if sys.platform != "darwin" or os.geteuid() != 0:
        parser.error("guest provisioning requires root on macOS")
    manifest = json.loads((HERE / "toolchain.json").read_text())
    if args.operation == "provision":
        provision(manifest, args.scratch, args.measure)
    else:
        seal(manifest, args.build_user, args.scratch)
