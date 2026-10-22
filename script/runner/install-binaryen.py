#!/usr/bin/env python3
"""Install the SHA-256-pinned Binaryen release used by the runner fleet."""

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


BUILD = Path("/srv/build")
MANIFEST = Path(__file__).with_name("toolchain.json")


def run(*args, capture=False):
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def sha256(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def source_spec(manifest):
    spec = manifest["tools"]["wasm-opt"]
    match = re.fullmatch(r"wasm-opt version (\d+)", spec.get("version", ""))
    if not match:
        raise ValueError("Binaryen version must be an exact wasm-opt major release")
    release = match.group(1)
    source = spec.get("source", {})
    expected_url = (f"https://github.com/WebAssembly/binaryen/releases/download/version_{release}/"
                    f"binaryen-version_{release}-x86_64-linux.tar.gz")
    if (source.get("url") != expected_url
            or not re.fullmatch(r"[0-9a-f]{64}", source.get("sha256", ""))
            or source.get("archive_root") != f"binaryen-version_{release}"
            or source.get("install_dir") != f"/usr/local/lib/binaryen/{release}"):
        raise ValueError("invalid Binaryen release archive pin")
    if (spec.get("path_by_role", {}).get("boat") != "/usr/local/bin/wasm-opt"
            or spec.get("path_by_role", {}).get("tenant") != "/usr/local/bin/wasm-opt"):
        raise ValueError("invalid Binaryen executable paths")
    return spec, source


def verify_archive(archive, root):
    with tarfile.open(archive, "r:gz") as bundle:
        names = bundle.getnames()
        binary = root + "/bin/wasm-opt"
        if binary not in names:
            raise ValueError("Binaryen release archive does not contain bin/wasm-opt")
        for member in bundle.getmembers():
            name = PurePosixPath(member.name)
            if name.is_absolute() or ".." in name.parts or name.parts[0] != root:
                raise ValueError("Binaryen archive contains an unsafe path")
            if member.issym() or member.islnk():
                target = posixpath.normpath(posixpath.join(posixpath.dirname(member.name), member.linkname))
                if target.startswith("/") or not target.startswith(root + "/"):
                    raise ValueError("Binaryen archive link escapes its release directory")


def install(manifest):
    if os.geteuid() != 0:
        raise PermissionError("Binaryen image installation must run as root")
    spec, source = source_spec(manifest)
    distro_binary = Path("/usr/bin/wasm-opt")
    if distro_binary.exists() or distro_binary.is_symlink():
        raise ValueError("distro wasm-opt remains; purge binaryen before installing the pinned release")
    version = spec["version"]
    archive_root = source["archive_root"]
    install_dir = Path(source["install_dir"])
    executable = install_dir / "bin/wasm-opt"
    link = Path(spec["path_by_role"]["boat"])
    if install_dir.exists() or install_dir.is_symlink():
        raise FileExistsError(f"Binaryen install directory already exists: {install_dir}")
    if link.exists() or link.is_symlink():
        raise FileExistsError(f"refusing to replace existing wasm-opt: {link}")
    work_root = BUILD / "tmp"
    work_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="binaryen-", dir=work_root) as temporary:
        work = Path(temporary)
        archive = work / "binaryen.tar.gz"
        urllib.request.urlretrieve(source["url"], archive)
        digest = sha256(archive)
        if digest != source["sha256"]:
            raise ValueError("Binaryen release SHA-256 mismatch")
        verify_archive(archive, archive_root)
        stage = work / "stage"
        stage.mkdir()
        old_umask = os.umask(0o022)
        try:
            run("tar", "--no-same-owner", "--no-same-permissions", "-xzf", str(archive),
                "-C", str(stage), "--strip-components=1")
        finally:
            os.umask(old_umask)
        staged_binary = stage / "bin/wasm-opt"
        if not staged_binary.is_file() or not os.access(staged_binary, os.X_OK):
            raise ValueError("Binaryen release does not contain an executable wasm-opt")
        install_dir.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(stage), install_dir)
    link.symlink_to(executable)
    actual = run(str(link), "--version", capture=True)
    expected_major = re.fullmatch(r"wasm-opt version (\d+)", version)
    actual_major = re.match(r"wasm-opt version (\d+)(?:\s|$)", actual)
    if (not expected_major or not actual_major or expected_major.group(1) != actual_major.group(1)
            or link.resolve() != executable):
        raise ValueError(f"installed wasm-opt major differs: expected {version!r}, observed {actual!r}")
    return {"path": str(link), "realpath": str(link.resolve()), "version": actual,
            "sha256": source["sha256"]}


def main():
    manifest_path = Path(sys.argv[1]) if len(sys.argv) == 2 else MANIFEST
    report = install(json.loads(manifest_path.read_text()))
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
