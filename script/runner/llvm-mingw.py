#!/usr/bin/env python3
"""The pinned, private ARM64 Windows C toolchain used by Linux job images."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = "/usr/local/lib/motor/llvm-mingw"
TARGET = "aarch64-pc-windows-gnullvm"
ROLES = ["boat", "tenant"]
ENVIRONMENT = {
    "CC_aarch64_pc_windows_gnullvm": ROOT + "/bin/aarch64-w64-mingw32-clang",
    "AR_aarch64_pc_windows_gnullvm": ROOT + "/bin/llvm-ar",
    "CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER": ROOT + "/bin/aarch64-w64-mingw32-clang",
}


def validate(manifest):
    spec = manifest.get("tools", {}).get("llvm-mingw", {})
    archive = spec.get("archive", {})
    release = archive.get("release", "")
    url = (f"https://github.com/mstorsjo/llvm-mingw/releases/download/{release}/"
           f"llvm-mingw-{release}-ucrt-ubuntu-22.04-x86_64.tar.xz")
    if (not re.fullmatch(r"\d{8}", release)
            or not re.fullmatch(r"\d+\.\d+\.\d+", spec.get("version", ""))
            or not re.fullmatch(r"[a-f0-9]{64}", archive.get("sha256", ""))
            or not re.fullmatch(r"https://api.github.com/repos/mstorsjo/llvm-mingw/releases/assets/\d+",
                                archive.get("digest_source", ""))
            or archive.get("url") != url or archive.get("format") != "tar.xz"
            or archive.get("strip_components") != 1
            or archive.get("install_dir") != ROOT
            or archive.get("install_path") != "bin/aarch64-w64-mingw32-clang"
            or spec.get("path") != ENVIRONMENT["CC_aarch64_pc_windows_gnullvm"]
            or spec.get("roles") != ROLES
            or spec.get("version_pattern") != "clang version " + re.escape(spec.get("version", "")) + r"(?: .*)?"):
        raise ValueError("invalid pinned llvm-mingw archive or install layout")
    if manifest.get("image_only_rust_targets") != [TARGET]:
        raise ValueError("invalid image-only Rust targets")
    for name in (manifest["rust_default"], "1.99.0-x86_64-unknown-linux-gnu"):
        if TARGET not in manifest["rust_toolchains"][name]["targets"]:
            raise ValueError("missing gnullvm Rust target: " + name)
    for name, value in ENVIRONMENT.items():
        if manifest.get("job_environment", {}).get(name) != value:
            raise ValueError("invalid llvm-mingw job environment: " + name)
    for environment in manifest["job_environment_by_role"].values():
        if ROOT + "/bin" in environment.get("PATH", "").split(":"):
            raise ValueError("llvm-mingw must not shadow distro tools on PATH")


def remove(path):
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()


def size_kib(root):
    return int(subprocess.check_output(["du", "-sk", str(root)], text=True).split()[0])


def prune(root, sha256):
    """Retain aarch64 wrappers, their LLVM dependencies and the shared headers."""
    before = size_kib(root)
    for path in root.iterdir():
        if path.name not in {"LICENSE.TXT", "aarch64-w64-mingw32", "generic-w64-mingw32", "bin", "lib"}:
            remove(path)
    binary_dir = root / "bin"
    keep = {path.name for path in binary_dir.glob("aarch64-w64-mingw32-*")}
    keep.update({"aarch64-pc-windows-gnu.cfg", "aarch64-w64-windows-gnu.cfg", "mingw32-common.cfg",
                 "clang", "ld.lld", "lld", "llvm-ar", "llvm-objdump", "llvm-rc", "llvm-cvtres"})
    # Wrapper aliases often point at another alias (llvm-ranlib -> llvm-ar).
    # Follow every retained symlink before removing the other architectures.
    pending = list(keep)
    while pending:
        path = binary_dir / pending.pop()
        if path.is_symlink():
            target = os.readlink(path)
            if Path(target).name != target:
                raise ValueError("unexpected llvm-mingw binary link: " + str(path))
            if target not in keep:
                keep.add(target)
                pending.append(target)
    for path in binary_dir.iterdir():
        if path.name not in keep:
            remove(path)
    for path in (root / "lib").iterdir():
        if path.name != "clang" and not path.name.startswith(("libLLVM.so", "libclang-cpp.so")):
            remove(path)
    for windows in (root / "lib/clang").glob("*/lib/windows"):
        for path in windows.iterdir():
            if "aarch64" not in path.name:
                remove(path)
    after = size_kib(root)
    print(f"llvm-mingw installed size: {before} KiB before pruning; {after} KiB after pruning", flush=True)
    if after > 600 * 1024:
        raise ValueError("pruned llvm-mingw exceeds 600 MiB")
    if not re.fullmatch(r"[a-f0-9]{64}", sha256):
        raise ValueError("invalid llvm-mingw archive receipt")
    (root / ".motor-archive-sha256").write_text(sha256 + "\n")
    return {"before_kib": before, "after_kib": after}


def probe(manifest, job=False, install_root=""):
    """Compile C including <assert.h>, archive its object and link a PE executable."""
    try:
        spec = manifest["tools"]["llvm-mingw"]
        root = Path(install_root + ROOT)
        if (root / ".motor-archive-sha256").read_text().strip() != spec["archive"]["sha256"]:
            raise ValueError("llvm-mingw archive receipt differs from the manifest pin")
        environment = os.environ if job else ENVIRONMENT
        commands = {}
        for name, expected in ENVIRONMENT.items():
            if environment.get(name) != expected:
                raise ValueError("missing or incorrect llvm-mingw job variable: " + name)
            commands[name] = install_root + expected
        with tempfile.TemporaryDirectory(prefix="llvm-mingw-probe-", dir=os.environ["TMPDIR"]) as temporary:
            work = Path(temporary)
            source, obj, library, exe = (work / name for name in ("probe.c", "probe.o", "probe.a", "probe.exe"))
            source.write_text("#include <assert.h>\nint main(void) { assert(sizeof(void *) == 8); return 0; }\n")
            for command in (
                [commands["CC_aarch64_pc_windows_gnullvm"], "-c", str(source), "-o", str(obj)],
                [commands["AR_aarch64_pc_windows_gnullvm"], "rcs", str(library), str(obj)],
                [commands["CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER"], str(obj), "-o", str(exe)],
            ):
                subprocess.run(command, check=True, capture_output=True, text=True, timeout=30)
            if obj.read_bytes()[:2] != b"\x64\xaa" or exe.read_bytes()[:2] != b"MZ":
                raise ValueError("llvm-mingw did not produce ARM64 COFF and a PE executable")
        return {"passed": True}
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        detail = (error.stderr or "") if isinstance(error, subprocess.CalledProcessError) else str(error)
        return {"passed": False, "error": detail[:2000]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prune", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    args = parser.parse_args()
    print(json.dumps(prune(args.prune, args.sha256), sort_keys=True))


if __name__ == "__main__":
    main()
