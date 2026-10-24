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
import signal
import subprocess

from build_storage import DiskMeter, FREE_FLOOR, GIB, assert_unmounted, choose_budget, read_mountinfo, unmount_tree

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
PARITY_FILES = tuple((REPO / "script/runner/parity-files.txt").read_text().splitlines())
BOOT_ARGS = ("console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro "
             "init=/sbin/motor-init net.ifnames=0 ipv6.disable=1 "
             "systemd.unified_cgroup_hierarchy=1")
RUNNER_INPUTS = (
    "script/runner/tools.txt", "script/runner/install-tools.sh",
    "script/runner/build-sccache.sh", "script/runner/motor-net.nft",
    "script/runner/patches/sccache-0.18.0-read-only-hits.patch",
    "script/runner/build-git.sh", "script/runner/install-binaryen.py",
    "script/runner/job-account.py",
    "script/runner/units/system/motor-jobs.slice",
    "script/runner/sysctl/60-motor-inotify.conf", "script/boat-image/install.py",
    "script/tenant-image/guest/root_paths.py",
) + tuple(f"script/runner/{name}" for name in PARITY_FILES)


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
    # Cleanup needs fchmodat2 with AT_EMPTY_PATH (Linux 6.6+). Firecracker's
    # supported guest series with that syscall starts at 6.18, not 6.12.
    if tuple(map(int, pins['kernel'].split('.'))) < (6, 18, 0):
        raise ValueError('kernel must be >=6.18.0 for a Firecracker-supported guest with fchmodat2(AT_EMPTY_PATH)')
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


def build_artifacts(stage, pins, runner, jobs, run=subprocess.run):
    if platform.system() != "Linux" or platform.machine() != "x86_64" or os.geteuid() != 0:
        raise ValueError("image builds need root on a dedicated Linux x86_64 operator host")
    stage_inputs(stage, pins, runner)
    env = kernel_env(pins, jobs)
    run(["bash", str(HERE / "build-kernel.sh"), str(stage / "kernel")], check=True, env=env)
    run(["unshare", "--mount", "--propagation", "private", "--", "bash", str(HERE / "build-rootfs.sh"), str(stage)], check=True, env=env)


def recipe_digest(pins, runner, jobs):
    files = [HERE / name for name in ("build.py", "build_storage.py", "build-kernel.sh", "build-rootfs.sh", "install-guest.py", "kernel-options.txt")]
    files.extend(path for path in (HERE / "guest").iterdir() if path.is_file())
    recipe = dict(pins=pins, runner_sha256=digest(runner), jobs=jobs,
                  sources={str(path.relative_to(HERE)): digest(path) for path in sorted(files)})
    return hashlib.sha256(json.dumps(recipe, sort_keys=True).encode()).hexdigest()


def build(version, output, versions, runner, jobs=4, builder=build_artifacts,
          build_budget=None, peak_profile=None, free_floor=FREE_FLOOR, mountinfo_reader=read_mountinfo):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,95}", version) or jobs < 1:
        raise ValueError("version must be one safe path component and jobs must be positive")
    output.mkdir(parents=True, exist_ok=True)
    target = output / version
    # mkdir is the reservation: simultaneous builders cannot replace each other.
    target.mkdir()
    stage = target / ".work"
    stage.mkdir()
    meter = None
    try:
        shutil.copy2(versions, stage / "versions.txt")
        pins = load_versions(stage / "versions.txt")
        recipe = recipe_digest(pins, runner, jobs)
        budget, source = choose_budget(build_budget, peak_profile, recipe)
        free = shutil.disk_usage(target).free
        print(f"image storage preflight: free={free}, floor={free_floor}, admitted cap={budget}; {source}", flush=True)
        if free < free_floor + budget:
            raise ValueError(f"image build needs floor + admitted budget: {free_floor + budget} bytes, available {free}")
        meter = DiskMeter(target, budget, recipe, floor=free_floor, reader=mountinfo_reader)
        meter.start()
        if builder is build_artifacts:
            builder(stage, pins, runner.resolve(), jobs, run=meter.run)
        else:
            builder(stage, pins, runner.resolve(), jobs)
        meter.checkpoint("artifacts built")
        check_kernel(stage / "kernel/kernel.config")
        assert_unmounted(stage, mountinfo_reader)
        manifest = {
            "version": version, "versions": pins, "boot_args": BOOT_ARGS,
            "capabilities": ["sparse-v1", "resize-v1"],
            "kernel_sha256": digest(stage / "kernel/vmlinux"),
            "rootfs_sha256": digest(stage / "rootfs"),
            "runner_sha256": digest(stage / "motor-runner"),
        }
        for source, name in (("kernel/vmlinux", "vmlinux"), ("rootfs", "rootfs"),
                             ("kernel/kernel.config", "kernel.config"),
                             ("packages.txt", "packages.txt"), ("tools.json", "tools.json")):
            (stage / source).rename(target / name)
        shutil.copy2(stage / "versions.txt", target / "versions.txt")
        meter.checkpoint("before stage removal")
        meter.stop()
        # Recheck immediately before recursive removal, even after a successful
        # rootfs cleanup. Never traverse a late or stacked mount during deletion.
        assert_unmounted(stage, mountinfo_reader)
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
        meter.finish("success")
    except BaseException:
        (target / "manifest.json").unlink(missing_ok=True)
        if meter is not None:
            try:
                meter.finish("failed")
            except Exception as error:
                print(f"storage measurement failure: {type(error).__name__}: {error}", flush=True)
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
    create.add_argument("--output-root", type=Path, default=Path("/var/lib/motor-vm/tenant-images"))
    create.add_argument("--runner", type=Path, required=True)
    create.add_argument("--jobs", type=int, default=4)
    storage = create.add_mutually_exclusive_group(required=True)
    storage.add_argument("--build-budget-gib", type=int, help="explicit first-build cap, not a measured requirement")
    storage.add_argument("--peak-profile", type=Path, help="matching successful disk-usage.json; adds 25%% plus 4 GiB")
    for sub in (create, subs.add_parser("inventory")):
        sub.add_argument("--versions", type=Path, default=HERE / "versions.txt")
    subs.choices["inventory"].add_argument("image", type=Path)
    subs.add_parser("check-kernel").add_argument("config", type=Path)
    config = subs.add_parser("kernel-config")
    config.add_argument("output", type=Path)
    config.add_argument("--versions", type=Path, default=HERE / "versions.txt")
    config.add_argument("--jobs", type=int, default=4)
    subs.add_parser("cleanup-mounts").add_argument("root", type=Path)
    subs.add_parser("check-mounts").add_argument("root", type=Path)
    args = parser.parse_args()
    if args.command == "build":
        def interrupted(signum, _frame):
            raise RuntimeError(f"image build interrupted by signal {signum}")
        previous = signal.signal(signal.SIGTERM, interrupted)
        try:
            build(args.version, args.output_root.resolve(), args.versions.resolve(), args.runner.resolve(), args.jobs,
                  build_budget=args.build_budget_gib * GIB if args.build_budget_gib is not None else None,
                  peak_profile=args.peak_profile)
        finally:
            signal.signal(signal.SIGTERM, previous)
    elif args.command == "inventory":
        inventory(args.image, args.versions)
    elif args.command == "kernel-config":
        pins = load_versions(args.versions)
        args.output.mkdir(parents=True)
        subprocess.run(["bash", str(HERE / "build-kernel.sh"), str(args.output.resolve()), "--config-only"],
                       check=True, env=kernel_env(pins, args.jobs))
    elif args.command == "cleanup-mounts":
        root = args.root.resolve()
        if root.name != "root" or root.parent.name != ".work" or os.geteuid() != 0:
            parser.error("internal cleanup requires root and VERSION/.work/root")
        unmount_tree(root)
    elif args.command == "check-mounts":
        assert_unmounted(args.root.resolve())
        print(f"stage mount fence: no mounts below {args.root.resolve()}")
    else:
        check_kernel(args.config)


if __name__ == "__main__":
    main()
