#!/usr/bin/env python3
"""Fetch the pinned prebuilt Rust fetch-only toolkit on Linux, never on the Windows host."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import tarfile
import urllib.request

def digest_file(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()

PINS = json.loads(Path("script/runner/toolchain.json").read_text())["windows_cross"]["prep_toolchain"]
VERSION = PINS["version"]
TARGET = PINS["target"]
ROOT = Path("target/windows-cross/prep-toolchain")
DOWNLOADS = Path("target/windows-cross/rust-downloads")
ROOT.mkdir(parents=True, exist_ok=True)
DOWNLOADS.mkdir(parents=True, exist_ok=True)
records = []
for component in ("cargo", "rustc", "rust-std"):
    name = f"{component}-{VERSION}-{TARGET}.tar.xz"
    url = f"https://static.rust-lang.org/dist/{name}"
    expected = PINS["components"][component]
    published = urllib.request.urlopen(url + ".sha256", timeout=60).read().decode().split()[0]
    if published != expected:
        raise SystemExit(f"Official Rust component changed from the reviewed pin: {name}")
    if len(expected) != 64 or any(c not in "0123456789abcdef" for c in expected):
        raise SystemExit("Invalid official Rust artifact hash")
    path = DOWNLOADS / name
    if not path.exists() or digest_file(path) != expected:
        with urllib.request.urlopen(url, timeout=120) as source, path.open("wb") as output:
            shutil.copyfileobj(source, output)
    if digest_file(path) != expected:
        raise SystemExit(f"Rust component hash mismatch: {name}")
    with tarfile.open(path, "r:xz") as archive:
        for member in archive:
            parts = PurePosixPath(member.name).parts
            if len(parts) < 3:
                continue
            if parts[1] != component and not (component == "rust-std" and parts[1] == f"rust-std-{TARGET}"):
                continue
            relative = PurePosixPath(*parts[2:])
            if relative.is_absolute() or ".." in relative.parts or member.issym() or member.islnk():
                raise SystemExit("Unexpected path or link in Rust distribution")
            destination = ROOT.joinpath(*relative.parts)
            if member.isdir():
                destination.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                destination.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as source, destination.open("wb") as output:
                    shutil.copyfileobj(source, output)
    records.append(dict(component=component, version=VERSION, target=TARGET, sha256=expected, url=url))
    print(f"Fetched official prebuilt {component} {VERSION} SHA256={expected}", flush=True)
(ROOT / "distribution.json").write_text(json.dumps(records, indent=2) + "\n")
for binary in ("cargo.exe", "rustc.exe"):
    path = ROOT / "bin" / binary
    if not path.is_file():
        raise SystemExit(f"Missing prebuilt toolkit executable {path}")
    with path.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    if digest != PINS[binary.replace('.exe', '_exe_sha256')]:
        raise SystemExit(f"Prebuilt toolkit executable changed: {binary}")
    print(f"TOOLKIT {binary} SHA256={digest}")
