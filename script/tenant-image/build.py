#!/usr/bin/env python3
"""Build immutable tenant images; inspect real artifacts with the inventory command."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
BOOT_ARGS = ("console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro "
             "init=/sbin/motor-init net.ifnames=0 ipv6.disable=1 "
             "systemd.unified_cgroup_hierarchy=1")
RUNNER_INPUTS = (
    "script/runner/tools.txt", "script/runner/install-tools.sh",
    "script/runner/build-sccache.sh", "script/runner/motor-net.nft",
    "script/runner/patches/sccache-0.18.0-read-only-hits.patch",
)


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def load_versions(path):
    def unique_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate versions key: {key}")
            result[key] = value
        return result

    pins = json.loads(path.read_text(), object_pairs_hook=unique_pairs)
    if pins["schema"] != 1 or pins["pnpm"] != "11.1.2":
        raise ValueError("unsupported versions schema or pnpm version")
    for name in ("node", "npm", "bun", "pnpm", "rust_default", "kernel", "kernel_gcc", "rustup"):
        if not re.fullmatch(r"\d+\.\d+\.\d+", pins[name]):
            raise ValueError(f"invalid release pin: {name}")
    if pins["rust_default"] not in pins["rust_toolchains"]:
        raise ValueError("default Rust must be an installed pinned toolchain")
    for toolchain, targets in pins["rust_toolchains"].items():
        if not re.fullmatch(r"\d+\.\d+\.\d+", toolchain) or not targets:
            raise ValueError("Rust toolchain pins need release versions and targets")
        if not all(re.fullmatch(r"[a-z0-9_]+(?:-[a-z0-9_]+)+", target) for target in targets):
            raise ValueError("invalid Rust target")
    if pins["debian_suite"] != "trixie" or not re.fullmatch(r"\d{8}T\d{6}Z", pins["debian_snapshot"]):
        raise ValueError("expected a pinned Debian trixie snapshot")
    if not isinstance(pins["rootfs_gib"], int) or not 16 <= pins["rootfs_gib"] <= 128:
        raise ValueError("rootfs_gib must be 16..128")
    if not pins["packages"] or not all(re.fullmatch(r"[a-z0-9][a-z0-9+.-]+", p) for p in pins["packages"]):
        raise ValueError("invalid package list")
    hashes = pins["runner_inputs_sha256"]
    if set(hashes) != set(RUNNER_INPUTS):
        raise ValueError("runner input pins must cover the complete installer and network inputs")
    for name, value in {"kernel": pins["kernel_sha256"], **hashes}.items():
        if not re.fullmatch(r"[0-9a-f]{64}", value):
            raise ValueError(f"invalid SHA-256: {name}")
    return pins


def check_kernel(path):
    resolved = set(path.read_text().splitlines())
    errors = []
    options = (HERE / "kernel-options.txt").read_text().splitlines()
    for option in options:
        if not option or option.startswith("#"):
            continue
        name, value = option.split("=")
        if value == "y" and f"CONFIG_{name}=y" not in resolved:
            errors.append(f"CONFIG_{name} must be built in")
        if value == "n" and any(f"CONFIG_{name}={v}" in resolved for v in ("y", "m")):
            errors.append(f"CONFIG_{name} must be disabled")
    if errors:
        raise ValueError("; ".join(errors))
    print(f"kernel config: {sum('=' in line for line in options)} checks passed")


def checked_copy(source, target, expected):
    if digest(source) != expected:
        raise ValueError(f"pinned input changed: {source}")
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)


def stage_inputs(stage, pins, runner):
    for name in RUNNER_INPUTS:
        checked_copy(REPO / name, stage / name, pins["runner_inputs_sha256"][name])
    shutil.copytree(HERE / "guest", stage / "guest")
    shutil.copy2(runner, stage / "motor-runner")
    (stage / "versions.json").write_text(json.dumps(pins))


def kernel_env(pins, jobs):
    return dict(os.environ, KERNEL_VERSION=pins["kernel"], KERNEL_SHA256=pins["kernel_sha256"],
               KERNEL_GCC=pins["kernel_gcc"], JOBS=str(jobs),
               RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="")


def build_artifacts(stage, pins, runner, jobs):
    if platform.system() != "Linux" or platform.machine() != "x86_64" or os.geteuid() != 0:
        raise ValueError("image builds need root on a dedicated Linux x86_64 operator host")
    stage_inputs(stage, pins, runner)
    env = kernel_env(pins, jobs)
    subprocess.run(["bash", str(HERE / "build-kernel.sh"), str(stage / "kernel")], check=True, env=env)
    subprocess.run(["bash", str(HERE / "build-rootfs.sh"), str(stage)], check=True, env=env)


def build(version, output, versions, runner, jobs=4, builder=build_artifacts):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,95}", version) or jobs < 1:
        raise ValueError("version must be one safe path component and jobs must be positive")
    output.mkdir(parents=True, exist_ok=True)
    target = output / version
    # mkdir is the reservation: simultaneous builders cannot replace each other.
    target.mkdir()
    stage = target / ".work"
    stage.mkdir()
    try:
        shutil.copy2(versions, stage / "versions.txt")
        pins = load_versions(stage / "versions.txt")
        builder(stage, pins, runner.resolve(), jobs)
        check_kernel(stage / "kernel/kernel.config")
        manifest = {
            "version": version, "versions": pins, "boot_args": BOOT_ARGS,
            "kernel_sha256": digest(stage / "kernel/vmlinux"),
            "rootfs_sha256": digest(stage / "rootfs"),
            "runner_sha256": digest(stage / "motor-runner"),
        }
        for source, name in (("kernel/vmlinux", "vmlinux"), ("rootfs", "rootfs"),
                             ("kernel/kernel.config", "kernel.config"),
                             ("packages.txt", "packages.txt"), ("tools.json", "tools.json")):
            (stage / source).rename(target / name)
        shutil.copy2(stage / "versions.txt", target / "versions.txt")
        # The rootfs builder must have unmounted its chroot before returning success.
        shutil.rmtree(stage)
        (target / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        for path in target.iterdir():
            path.chmod(0o444)
            with path.open("rb") as source:
                os.fsync(source.fileno())
        fd = os.open(target, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    except BaseException:
        print(f"incomplete version retained at {target}; inspect mounts before removing it", flush=True)
        raise
    print(f"image build: {target} (kernel, rootfs, manifest, configuration and tool inventories)")
    return target


def inventory(image, versions):
    if (image / ".work").exists():
        raise ValueError("incomplete image build still has a staging directory")
    manifest = json.loads((image / "manifest.json").read_text())
    expected = load_versions(versions)
    if manifest["version"] != image.name or manifest["versions"] != expected:
        raise ValueError("image manifest version or toolchain inventory differs from versions.txt")
    if load_versions(image / "versions.txt") != expected:
        raise ValueError("image's versions.txt differs from requested versions.txt")
    for artifact, key in (("vmlinux", "kernel_sha256"), ("rootfs", "rootfs_sha256")):
        if digest(image / artifact) != manifest[key]:
            raise ValueError(f"{artifact} SHA-256 mismatch")
    check_kernel(image / "kernel.config")
    print("hosted image inventory: 5 checks passed (versions, embedded pins, kernel, rootfs, config)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest="command", required=True)
    create = subs.add_parser("build")
    create.add_argument("version")
    create.add_argument("--output-root", type=Path, default=Path("/srv/build/vm/tenant-images"))
    create.add_argument("--runner", type=Path, required=True)
    create.add_argument("--jobs", type=int, default=4)
    for sub in (create, subs.add_parser("inventory")):
        sub.add_argument("--versions", type=Path, default=HERE / "versions.txt")
    subs.choices["inventory"].add_argument("image", type=Path)
    subs.add_parser("check-kernel").add_argument("config", type=Path)
    config = subs.add_parser("kernel-config")
    config.add_argument("output", type=Path)
    config.add_argument("--versions", type=Path, default=HERE / "versions.txt")
    config.add_argument("--jobs", type=int, default=4)
    args = parser.parse_args()
    if args.command == "build":
        build(args.version, args.output_root.resolve(), args.versions.resolve(), args.runner.resolve(), args.jobs)
    elif args.command == "inventory":
        inventory(args.image, args.versions)
    elif args.command == "kernel-config":
        pins = load_versions(args.versions)
        args.output.mkdir(parents=True)
        subprocess.run(["bash", str(HERE / "build-kernel.sh"), str(args.output.resolve()), "--config-only"],
                       check=True, env=kernel_env(pins, args.jobs))
    else:
        check_kernel(args.config)


if __name__ == "__main__":
    main()
