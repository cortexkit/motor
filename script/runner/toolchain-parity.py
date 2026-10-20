#!/usr/bin/env python3
"""Read-only inventory of the Linux runner's job-visible toolchain."""

import argparse
import hashlib
import json
import re
import sys
import os
from pathlib import Path
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
MANIFEST = HERE / "toolchain.json"


def output(*args, env=None):
    result = subprocess.run(args, env=env, text=True, capture_output=True, check=True, timeout=30)
    return result.stdout.strip()


def binary_version(path, env):
    # sccache may contact a long-lived daemon even for informational commands.
    # Inventory must never start or depend on a cache server.
    if Path(path).name.startswith("sccache"):
        return {"version": None, "error": "not-probed: sccache"}
    if Path(path).name == "gnutest":
        return {"version": None, "error": "not-probed: test has no version flag"}
    try:
        name = Path(path).name
        # true and false recognize --version only under their own names.
        argv0 = name[3:] if name.startswith("gnu") else str(path)
        result = subprocess.run([argv0, "--version"], executable=str(path), env=env,
                                text=True, capture_output=True, timeout=30)
        return {"version": (result.stdout + result.stderr).strip(), "exit_code": result.returncode}
    except subprocess.TimeoutExpired:
        return {"version": None, "error": "timeout"}
    except OSError as error:
        return {"version": None, "error": str(error)}


def capture(manifest=None, job=False):
    paths = manifest or {}
    cargo_home = Path(paths.get("cargo_home", os.environ.get("CARGO_HOME", "/srv/build/cargo-home")))
    rustup_home = Path(paths.get("rustup_home", os.environ.get("RUSTUP_HOME", "/home/ubuntu/.rustup")))
    env = dict(os.environ, CARGO_HOME=str(cargo_home), RUSTUP_HOME=str(rustup_home),
               RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1")
    if not job:
        env["PATH"] = str(cargo_home / "bin") + ":/home/ubuntu/.bun/bin:" + os.environ.get("PATH", os.defpath)
    rustup = str(cargo_home / "bin/rustup")
    toolchains = {}
    target_payloads = {}
    for line in output(rustup, "toolchain", "list", env=env).splitlines():
        name = line.split()[0]
        toolchains[name] = {
            "rustc": output(str(rustup_home / "toolchains" / name / "bin/rustc"), "--version", env=env),
            "targets": sorted(output(rustup, "target", "list", "--installed", "--toolchain", name, env=env).splitlines()),
            "components": sorted(output(rustup, "component", "list", "--installed", "--toolchain", name, env=env).splitlines()),
        }
        target_payloads[name] = {
            target: any(path.is_file() and path.stat().st_size > 0 for path in
                        (rustup_home / "toolchains" / name / "lib/rustlib" / target / "lib").glob("libstd-*.rlib"))
            for target in toolchains[name]["targets"]}
    binaries = {}
    for path in sorted((cargo_home / "bin").iterdir()):
        if path.is_file() and os.access(path, os.X_OK) and not path.name.startswith("."):
            binaries[path.name] = {"symlink": os.readlink(path) if path.is_symlink() else None}
            if not os.path.samefile(path, cargo_home / "bin/rustup"):
                binaries[path.name].update(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                                           **binary_version(path, env))
    metadata = {}
    for root in ([] if job else [cargo_home, *sorted(Path("/srv/build/tools").glob("*"))]):
        path = root / ".crates2.json"
        if path.is_file():
            metadata[str(root)] = json.loads(path.read_text())
    tools = {}
    names = {"node", "npm", "bun", "pnpm", "nats-server", "wasm-opt", "esbuild",
             "x86_64-w64-mingw32-gcc", "x86_64-w64-mingw32-g++"}
    names.update(paths.get("gnu_coreutils", {}))
    for directory in env["PATH"].split(":"):
        if directory:
            names.update(path.name for path in Path(directory).glob("gnu*") if path.is_file())
    for name in sorted(names):
        path = shutil.which(name, path=env["PATH"])
        if path is None and name in paths.get("gnu_coreutils", {}):
            candidate = paths["gnu_coreutils"][name]
            path = candidate if os.access(candidate, os.X_OK) else None
        if path:
            owner = "" if job else subprocess.run(
                ["dpkg-query", "-S", str(Path(path).resolve())], text=True,
                capture_output=True, timeout=30).stdout.strip()
            tools[name] = {"path": path, "realpath": str(Path(path).resolve()),
                           **binary_version(path, env), "package": owner}
            if name == 'gnutest':
                tools[name]['sha256'] = hashlib.sha256(Path(path).read_bytes()).hexdigest()
        else:
            tools[name] = None
    packages = {}
    for line in ([] if job else output("dpkg-query", "-W", "-f=${Package}\t${Version}\t${db:Status-Status}\\n").splitlines()):
        name, version, status = line.split("\t")
        if status == "installed":
            packages[name] = version
    return {
        "schema": 1,
        "os_release": "" if job else Path("/etc/os-release").read_text(),
        "rustup_version": output(rustup, "--version", env=env),
        "rust_default": output(rustup, "default", env=env).split()[0],
        "rust_profile": output(rustup, "show", "profile", env=env),
        "rust_toolchains": toolchains,
        "rust_target_payloads": target_payloads,
        "cargo_home": str(cargo_home), "rustup_home": str(rustup_home),
        "cargo_binaries": binaries, "cargo_metadata": metadata,
        "tools": tools, "apt_packages": packages,
        "environment": {name: env.get(name) for name in (
            "PATH", "CC", "CXX", "WASM_BINDGEN_BIN", "WASM_OPT_BIN", "ESBUILD_BIN",
            "RUSTFLAGS", "CARGO_BUILD_TARGET", "PKG_CONFIG_PATH")},
        "job_environment": {name: os.environ.get(name) for name in paths.get("job_environment", {})},
        "mountinfo": Path("/proc/self/mountinfo").read_text() if job else "",
    }


def first_line(text):
    return text.splitlines()[0] if text else ""


def validate(manifest):
    if manifest["schema"] != 1 or manifest["rust_default"] not in manifest["rust_toolchains"]:
        raise ValueError("invalid parity schema or default toolchain")
    for key in ("rustup",):
        if not re.fullmatch(r"\d+\.\d+\.\d+", manifest[key]):
            raise ValueError("unpinned version: " + key)
    for name, spec in manifest["rust_toolchains"].items():
        if not re.fullmatch(r"(?:\d+\.\d+\.\d+|stable)-x86_64-unknown-linux-gnu", name):
            raise ValueError("unpinned Rust toolchain: " + name)
        if not re.fullmatch(r"rustc \d+\.\d+\.\d+ \([a-f0-9]+ \d{4}-\d{2}-\d{2}\)", spec["rustc"]):
            raise ValueError("unpinned rustc: " + name)
        if not spec["targets"] or spec["targets"] != sorted(set(spec["targets"])):
            raise ValueError("invalid Rust targets: " + name)
        std = sorted(component.removeprefix("rust-std-") for component in spec["components"] if component.startswith("rust-std-"))
        if std != spec["targets"]:
            raise ValueError("Rust targets and components disagree: " + name)
    for name, pattern in {"node": r"v\d+\.\d+\.\d+", "npm": r"\d+\.\d+\.\d+",
                          "bun": r"\d+\.\d+\.\d+", "pnpm": r"\d+\.\d+\.\d+",
                          "nats-server": r"nats-server: v\d+\.\d+\.\d+"}.items():
        if not re.fullmatch(pattern, manifest["tools"][name]["version"]):
            raise ValueError("unpinned version: " + name)
    defaults = set()
    for tool in manifest["cargo_installs"]:
        if "git" in tool:
            if not re.fullmatch(r"[0-9a-f]{40}", tool["rev"]):
                raise ValueError("git tools need a full revision")
            if tool.get("default"):
                if tool["crate"] in defaults:
                    raise ValueError("duplicate default Cargo tool")
                defaults.add(tool["crate"])
        elif not re.fullmatch(r"\d+\.\d+\.\d+", tool["version"]):
            raise ValueError("unpinned Cargo tool")
        for name in tool["bins"]:
            binary = name + "-" + tool["rev"][:8] if "git" in tool else name
            expected = manifest["cargo_binaries"][binary]
            if "git" not in tool and tool["version"] not in expected.get("version", "").split():
                raise ValueError("Cargo install and binary versions disagree: " + name)


def readonly_mount(path, mountinfo):
    matches = [line.split() for line in mountinfo.splitlines()
               if path == line.split()[4] or path.startswith(line.split()[4].rstrip("/") + "/")]
    return bool(matches) and "ro" in max(matches, key=lambda row: len(row[4]))[5].split(",")


def differences(manifest, actual, role="build-server", job=False, gnu_test_sha256=None):
    errors = []

    def equal(name, expected, observed):
        if expected != observed:
            errors.append(f"{name}: expected {expected!r}, observed {observed!r}")

    for key in ("rust_default", "rust_profile", "cargo_home", "rustup_home"):
        equal(key, manifest[key], actual.get(key))
    chains = actual.get("rust_toolchains", {})
    equal("Rust toolchain names", sorted(manifest["rust_toolchains"]), sorted(chains))
    for name, spec in manifest["rust_toolchains"].items():
        for key in ("rustc", "targets", "components"):
            equal(name + " " + key, spec[key], chains.get(name, {}).get(key))
        # rustup's receipts can survive a deleted or incomplete target directory.
        for target in spec['targets']:
            if actual.get('rust_target_payloads', {}).get(name, {}).get(target) is not True:
                errors.append(name + ': missing libstd payload for ' + target)
    version = first_line(actual.get("rustup_version", "")).split()
    equal("rustup", manifest["rustup"], version[1] if len(version) > 1 else None)
    expected_bins, bins = manifest["cargo_binaries"], actual.get("cargo_binaries", {})
    equal("cargo binary names", sorted(expected_bins), sorted(bins))
    for name, expected in expected_bins.items():
        observed = bins.get(name, {})
        if expected.get("proxy"):
            equal(name + " rustup proxy", "rustup", observed.get("symlink"))
        elif "version" in expected:
            equal(name + " version", expected["version"], first_line(observed.get("version")))
            equal(name + " --version exit", 0, observed.get("exit_code"))
        if "matches" in expected:
            other = bins.get(expected["matches"], {})
            if not observed.get("sha256") or observed.get("sha256") != other.get("sha256"):
                errors.append(name + ": does not match " + expected["matches"])
    if not bins.get("sccache", {}).get("sha256") or bins.get("sccache", {}).get("sha256") == bins.get("sccache-0.18.0-stock", {}).get("sha256"):
        errors.append("sccache: missing patched binary or identical to stock")
    if role == "build-server":
        for name, expected in manifest["sccache"]["build_server_sha256"].items():
            equal(name + " SHA-256", expected, bins.get(name, {}).get("sha256"))
    tools = actual.get("tools", {})
    for name, expected in manifest["tools"].items():
        observed = tools.get(name) or {}
        equal(name + " path", expected["path"], observed.get("path"))
        equal(name + " --version exit", 0, observed.get("exit_code"))
        version = first_line(observed.get("version"))
        if "version" in expected:
            equal(name + " version", expected["version"], version)
        elif not re.fullmatch(expected["version_pattern"], version):
            errors.append(name + ": unexpected distro tool version " + repr(version))
        if "realpath_suffix" in expected and not observed.get("realpath", "").endswith(expected["realpath_suffix"]):
            errors.append(name + ": wrong compiler thread model")
    for name in manifest["gnu_coreutils"]:
        observed = tools.get(name) or {}
        equal(name + " path", manifest["gnu_coreutils"][name], observed.get("path"))
        packages = {owner.split(':', 1)[0] for line in observed.get('package', '').splitlines()
                    for owner in line.partition(': ')[0].split(', ') if owner}
        owned = bool(packages & {'coreutils', 'gnu-coreutils'})
        family = bool(re.fullmatch(r'.*\((?:GNU )?coreutils\) [0-9]+(?:\.[0-9]+)+.*',
                                  first_line(observed.get('version'))))
        if name == 'gnutest' and job:
            # Job filesystems omit dpkg's database. Compare gnutest's hash with
            # the fresh host hash after checking that host file's package owner.
            # test has no --version option.
            identified = bool(gnu_test_sha256) and observed.get('sha256') == gnu_test_sha256
        else:
            identified = owned or (name != 'gnutest' and not packages and family)
        if not observed.get("path") or not identified:
            errors.append(name + ": missing GNU coreutils alias")
    if job:
        for name, expected in manifest["job_environment"].items():
            equal("job " + name, expected, actual.get("job_environment", {}).get(name))
        if not readonly_mount(manifest["rustup_home"], actual.get("mountinfo", "")):
            errors.append("job rustup home is not mounted read-only")
    else:
        required = manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"][role]
        for name in required:
            if name not in actual.get("apt_packages", {}):
                errors.append("missing apt package: " + name)
        if role == "boat":
            equal("sccache build inputs", manifest["sccache"]["inputs_sha256"], actual.get("sccache_inputs_sha256"))
            artifacts = actual.get('sccache_artifacts') or {}
            equal('sccache artifact inputs', manifest['sccache']['inputs_sha256'], artifacts.get('inputs_sha256'))
            for name in manifest['sccache']['build_server_sha256']:
                equal(name + ' built artifact hash', artifacts.get('binaries', {}).get(name), bins.get(name, {}).get('sha256'))
    return errors


def check(manifest, role="build-server", job=False, gnu_test_sha256=None):
    validate(manifest)
    actual = capture(manifest, job=job)
    if role == "boat" and not job:
        marker = Path("/srv/build/sccache-inputs.sha256")
        actual["sccache_inputs_sha256"] = marker.read_text() if marker.is_file() else None
        artifacts = Path('/srv/build/sccache-artifacts.json')
        actual['sccache_artifacts'] = json.loads(artifacts.read_text()) if artifacts.is_file() else None
    errors = differences(manifest, actual, role, job, gnu_test_sha256)
    # Keep qualification output bounded without discarding the measured versions.
    actual.pop("cargo_metadata", None)
    for row in [*actual["cargo_binaries"].values(), *actual["tools"].values()]:
        if row and row.get("version"):
            row["version"] = first_line(row["version"])
    required = manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"][role]
    actual["apt_packages"] = {name: version for name, version in actual["apt_packages"].items() if name in required}
    if job:
        actual["mountinfo"] = "\n".join(line for line in actual["mountinfo"].splitlines()
                                          if manifest["rustup_home"] in line.split()[:6])
    encoded = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
    return {"passed": not errors, "errors": errors, "role": role, "job": job,
            "manifest_sha256": hashlib.sha256(encoded).hexdigest(), "observed": actual}


def refresh(manifest, actual):
    # Preserve install sources and accepted OS differences; refresh host measurements.
    manifest = json.loads(json.dumps(manifest))
    for key in ("rust_default", "rust_profile", "rust_toolchains", "cargo_home", "rustup_home"):
        manifest[key] = actual[key]
    manifest["rustup"] = actual["rustup_version"].split()[1]
    manifest["cargo_binaries"] = {}
    for name, row in actual["cargo_binaries"].items():
        expected: dict[str, object] = {"proxy": True} if row["symlink"] == "rustup" else {}
        if row.get("version"):
            if row.get("exit_code") != 0:
                raise ValueError("failed version capture: " + name)
            expected["version"] = first_line(row["version"])
        elif name not in ("rustup", "sccache", "sccache-0.18.0-stock") and not expected:
            raise ValueError("unmeasured binary: " + name)
        manifest["cargo_binaries"][name] = expected
    for install in manifest["cargo_installs"]:
        if install.get("default"):
            for name in install["bins"]:
                variant = name + "-" + install["rev"][:8]
                if actual["cargo_binaries"][name]["sha256"] != actual["cargo_binaries"][variant]["sha256"]:
                    raise ValueError("default binary does not match pinned revision: " + name)
                manifest["cargo_binaries"][name]["matches"] = variant
    for name, expected in manifest["tools"].items():
        row = actual["tools"][name]
        if not row or row.get("exit_code") != 0:
            raise ValueError("unmeasured tool: " + name)
        expected["path"] = row["path"]
        if "version" in expected:
            expected["version"] = first_line(row["version"])
        else:
            expected["observed_build_server_version"] = first_line(row["version"])
    manifest["gnu_coreutils"] = {name: row["path"] for name, row in actual["tools"].items()
                                 if row and row.get("package", "").startswith("gnu-coreutils:")}
    required = manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"]["build-server"]
    manifest["apt_packages"]["observed_build_server_versions"] = {name: actual["apt_packages"][name] for name in required}
    manifest["sccache"]["build_server_sha256"] = {
        name: actual["cargo_binaries"][name]["sha256"] for name in ("sccache", "sccache-0.18.0-stock")}
    manifest["sccache"]["inputs_sha256"] = "".join(hashlib.sha256((HERE / name).read_bytes()).hexdigest() for name in (
        "build-sccache.sh", "patches/sccache-0.18.0-read-only-hits.patch"))
    manifest["capture_sha256"] = hashlib.sha256(json.dumps(actual, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    validate(manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--capture", action="store_true",
                        help="print observed state as JSON; does not install or modify tools")
    modes.add_argument("--check", action="store_true", help="compare live state with the manifest; exit nonzero on drift")
    modes.add_argument("--from-capture", type=Path, help="refresh measured manifest fields from captured JSON to stdout")
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--role", choices=("build-server", "boat"), default="build-server")
    parser.add_argument("--job", action="store_true", help="check inside a real Rust job, including PATH and read-only rustup")
    parser.add_argument('--gnu-test-sha256', help='for --job: hash of gnutest from the fresh package-qualified host check')
    args = parser.parse_args()
    if args.capture:
        result = capture()
    else:
        manifest = json.loads(args.manifest.read_text())
        result = refresh(manifest, json.loads(args.from_capture.read_text())) if args.from_capture else check(
            manifest, args.role, args.job, args.gnu_test_sha256)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 1 if args.check and not result["passed"] else 0


if __name__ == "__main__":
    sys.exit(main())
