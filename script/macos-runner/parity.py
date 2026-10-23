"""Darwin inventory and validation, loaded only for the macos parity role."""

import hashlib
import json
import os
import plistlib
from pathlib import Path
import re
import shutil
import subprocess

HOST = "aarch64-apple-darwin"
PREFIX = "/opt/motor"
ENVIRONMENT = {
    "HOME": "/Users/motorjob",
    "TMPDIR": {"per_job_directory": True},
    "PATH": "/opt/motor/cargo/bin:/opt/motor/bun/bin:/opt/motor/bin:/usr/bin:/bin:/usr/sbin:/sbin",
    "RUSTUP_HOME": "/opt/motor/rustup",
    "CARGO_HOME": "/opt/motor/job/cargo-home",
    "CARGO_TARGET_DIR": None,
}


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def manifest_digest(manifest):
    return hashlib.sha256(json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def output(*args, env=None):
    return subprocess.run(args, env=env, text=True, capture_output=True,
                          check=True, timeout=30).stdout.strip()


def sha256(value):
    return isinstance(value, str) and re.fullmatch(r"[a-f0-9]{64}", value) is not None


def tree_digest(directory):
    result = hashlib.sha256()
    root = Path(directory)
    for path in sorted(root.rglob("*")):
        result.update(str(path.relative_to(root)).encode() + b"\0")
        if path.is_symlink():
            result.update(b"link\0" + os.readlink(path).encode())
        elif path.is_file():
            result.update(b"file\0" + bytes.fromhex(digest(path)))
        else:
            result.update(b"dir\0")
    return result.hexdigest()


def validate(manifest, server):
    if manifest.get("schema") != 1 or manifest.get("platform") != "macos":
        raise ValueError("invalid macOS parity schema")
    if manifest.get("release_status") != "reviewed":
        raise ValueError("macOS pin lock is pending measurement/review")
    if manifest.get("rustup") != server["rustup"]:
        raise ValueError("macOS rustup differs from build-server")
    if manifest.get("rust_profile") != server["rust_profile"]:
        raise ValueError("macOS Rust profile differs from build-server")
    expected = {}
    for name, spec in server["rust_toolchains"].items():
        version = name.removesuffix("-x86_64-unknown-linux-gnu")
        components = sorted(component.replace("x86_64-unknown-linux-gnu", HOST)
                            for component in spec["components"]
                            if not component.startswith("rust-std-")
                            or component == "rust-std-x86_64-unknown-linux-gnu")
        expected[version + "-" + HOST] = {
            "rustc": spec["rustc"], "targets": [HOST], "components": components}
    chains = manifest.get("rust_toolchains", {})
    projected = {name: {key: row.get(key) for key in ("rustc", "targets", "components")}
                 for name, row in chains.items()}
    if projected != expected:
        raise ValueError("macOS Rust toolchains/components differ from build-server")
    for name, row in chains.items():
        if not sha256(row.get("sha256")) or not row.get("rustc_verbose", "").startswith(row["rustc"] + "\n"):
            raise ValueError("missing measured Rust toolchain pin: " + name)
    default = server["rust_default"].removesuffix("-x86_64-unknown-linux-gnu") + "-" + HOST
    if manifest.get("rust_default") != default:
        raise ValueError("macOS Rust default differs from build-server")
    stable = expected["stable-" + HOST]
    numbered = stable["rustc"].split()[1] + "-" + HOST
    if manifest.get("stable_copy_from") != numbered or expected.get(numbered, {}).get("rustc") != stable["rustc"]:
        raise ValueError("stable must be copied from the pinned numbered release")
    if manifest.get("cargo_home") != PREFIX + "/cargo" or manifest.get("rustup_home") != PREFIX + "/rustup":
        raise ValueError("invalid macOS Rust installation paths")
    if manifest.get("job_environment_by_role", {}).get("macos") != ENVIRONMENT:
        raise ValueError("invalid pinned macOS job environment")
    expected_bins = {name for name in server["cargo_binaries"] if not name.startswith("sccache")}
    if set(manifest["cargo_binaries"]) != expected_bins:
        raise ValueError("macOS cargo-bin names differ from build-server (excluding Linux sccache)")
    installs = [{key: value for key, value in row.items() if key != "source"}
                for row in manifest["cargo_installs"]]
    if installs != [row for row in server["cargo_installs"] if row["crate"] != "sccache"]:
        raise ValueError("macOS Cargo installs differ from build-server")
    for name, spec in manifest["cargo_binaries"].items():
        if spec.get("proxy"):
            if server["cargo_binaries"].get(name) != spec:
                raise ValueError("invalid rustup proxy: " + name)
        else:
            if not sha256(spec.get("sha256")):
                raise ValueError("missing macOS cargo-bin SHA-256: " + name)
            if spec.get("version") != server["cargo_binaries"].get(name, {}).get("version"):
                raise ValueError("macOS cargo-bin version differs from build-server: " + name)
            if spec.get("matches") != server["cargo_binaries"][name].get("matches"):
                raise ValueError("macOS cargo-bin variant differs from build-server: " + name)
    if set(manifest["tools"]) != {"bun", "git", "bash"}:
        raise ValueError("invalid macOS tool set")
    for name in ("bun", "git", "bash"):
        spec = manifest["tools"][name]
        if not sha256(spec.get("sha256")) or not spec.get("version"):
            raise ValueError("missing macOS tool pin: " + name)
        if name in ("bun", "git") and spec["version"] != server["tools"][name]["version"]:
            raise ValueError("macOS tool version differs from build-server: " + name)
        if not spec["path"].startswith(PREFIX + "/"):
            raise ValueError("invalid macOS tool path: " + name)
    clt = manifest["clt"]
    if not clt.get("version") or clt.get("package_id") != "com.apple.pkg.CLTools_Executables":
        raise ValueError("missing Command Line Tools pin")


def capture(manifest, job=False):
    cargo_home = Path(manifest["cargo_home"])
    rustup_home = Path(manifest["rustup_home"])
    env = dict(os.environ, RUSTUP_HOME=str(rustup_home), CARGO_HOME=str(cargo_home),
               RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1")
    if not job:
        env["PATH"] = manifest["job_environment_by_role"]["macos"]["PATH"]
    rustup = str(cargo_home / "bin/rustup")
    toolchains, payloads = {}, {}
    for line in output(rustup, "toolchain", "list", env=env).splitlines():
        name = line.split()[0]
        toolchains[name] = {
            "rustc": output(str(rustup_home / "toolchains" / name / "bin/rustc"), "--version", env=env),
            "targets": sorted(output(rustup, "target", "list", "--installed", "--toolchain", name, env=env).splitlines()),
            "components": sorted(output(rustup, "component", "list", "--installed", "--toolchain", name, env=env).splitlines()),
            "rustc_verbose": output(str(rustup_home / "toolchains" / name / "bin/rustc"), "-vV", env=env),
            "sha256": tree_digest(rustup_home / "toolchains" / name),
        }
        payloads[name] = all(any(path.stat().st_size > 0 for path in
                                (rustup_home / "toolchains" / name / "lib/rustlib" / target / "lib").glob("libstd-*.rlib"))
                             for target in toolchains[name]["targets"])
    binaries = {}
    for path in sorted((cargo_home / "bin").iterdir()):
        if path.is_file() and os.access(path, os.X_OK) and not path.name.startswith("."):
            row = {"symlink": os.readlink(path) if path.is_symlink() else None}
            if not os.path.samefile(path, cargo_home / "bin/rustup"):
                row.update(sha256=digest(path), version=output(str(path), "--version", env=env).splitlines()[0])
            elif path.name == "rustup":
                row["sha256"] = digest(path)
            binaries[path.name] = row
    tools = {}
    for name in manifest["tools"]:
        path = shutil.which(name, path=env["PATH"])
        tools[name] = ({"path": path, "sha256": digest(path),
                        "version": output(path, "--version", env=env).splitlines()[0]} if path else None)
    stable = rustup_home / "toolchains" / ("stable-" + HOST)
    source = rustup_home / "toolchains" / manifest["stable_copy_from"]
    # Stable's cargo and rustc must be byte-identical to the pinned numbered release.
    stable_copied = not stable.is_symlink() and all(
        digest(stable / "bin" / name) == digest(source / "bin" / name) for name in ("cargo", "rustc"))
    receipt = output("/usr/sbin/pkgutil", "--pkg-info-plist", manifest["clt"]["package_id"])
    clt = plistlib.loads(receipt.encode())["pkg-version"]
    return {
        "rustup": output(rustup, "--version", env=env).split()[1],
        "rust_default": output(rustup, "default", env=env).split()[0],
        "rust_profile": output(rustup, "show", "profile", env=env),
        "rust_toolchains": toolchains, "rust_target_payloads": payloads,
        "cargo_binaries": binaries, "tools": tools, "clt": clt,
        "stable_copied": stable_copied,
        "rustup_writable": os.access(rustup_home, os.W_OK) or any(
            os.access(root, os.W_OK) or any(os.access(Path(root) / name, os.W_OK) for name in [*dirs, *files])
            for root, dirs, files in os.walk(rustup_home, onerror=raise_walk_error)),
        "job_environment": {name: os.environ.get(name) for name in ENVIRONMENT},
        "installed_manifest_sha256": manifest_digest(json.loads(Path(PREFIX + "/toolchain.json").read_text())),
    }


def raise_walk_error(error):
    raise error


def differences(manifest, actual, job=False):
    errors = []

    def equal(name, expected, observed):
        if expected != observed:
            errors.append(f"{name}: expected {expected!r}, observed {observed!r}")

    equal("sealed pin lock", manifest_digest(manifest), actual.get("installed_manifest_sha256"))
    for key in ("rustup", "rust_default", "rust_profile", "rust_toolchains"):
        equal(key, manifest[key], actual.get(key))
    equal("stable copied from pinned release", True, actual.get("stable_copied"))
    for name in manifest["rust_toolchains"]:
        equal(name + " libstd payload", True, actual.get("rust_target_payloads", {}).get(name))
    bins = actual.get("cargo_binaries", {})
    equal("cargo binary names", sorted(manifest["cargo_binaries"]), sorted(bins))
    for name, spec in manifest["cargo_binaries"].items():
        row = bins.get(name, {})
        if spec.get("proxy"):
            equal(name + " proxy", "rustup", row.get("symlink"))
        else:
            for key in (["sha256", "version"] if "version" in spec else ["sha256"]):
                equal(name + " " + key, spec[key], row.get(key))
            if spec.get("matches"):
                equal(name + " pinned variant", row.get("sha256"), bins.get(spec["matches"], {}).get("sha256"))
    for name, spec in manifest["tools"].items():
        row = actual.get("tools", {}).get(name) or {}
        for key in ("path", "version", "sha256"):
            equal(name + " " + key, spec[key], row.get(key))
    equal("Command Line Tools", manifest["clt"]["version"], actual.get("clt"))
    if job:
        for name, expected in manifest["job_environment_by_role"]["macos"].items():
            observed = actual.get("job_environment", {}).get(name)
            if isinstance(expected, dict) and expected.get("per_job_directory"):
                if (not observed or not Path(observed).is_absolute() or not Path(observed).is_dir()
                        or not Path(observed).name.startswith("motor-job-")):
                    errors.append("job TMPDIR is not an existing absolute per-job directory")
            else:
                equal("job " + name, expected, observed)
        equal("job rustup home writable", False, actual.get("rustup_writable"))
    return errors
