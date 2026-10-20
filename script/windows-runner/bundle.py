#!/usr/bin/env python3
"""Package our release binaries and prebuilt Rust toolkit, never Microsoft's SDK."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import zipfile

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "target/windows-release"
provenance_path = ROOT / 'target/windows-release-provenance.json'
if not provenance_path.is_file():
    raise SystemExit('Missing build provenance; run build-release.sh first')
provenance = json.loads(provenance_path.read_text())
if OUT.exists():
    shutil.rmtree(OUT)
(OUT / "bin").mkdir(parents=True)
for name in ("motor-runner", "motor-winagent"):
    source = ROOT / f"target/x86_64-pc-windows-msvc/release/{name}.exe"
    if not source.is_file():
        raise SystemExit(f"Missing cross-release artifact {source}; run build-release.sh first")
    shutil.copyfile(source, OUT / "bin" / source.name)
kit = ROOT / "target/windows-cross/prep-toolchain"
if not (kit / "bin/cargo.exe").is_file() or not (kit / "bin/rustc.exe").is_file():
    raise SystemExit("Missing pinned prebuilt fetch-only toolkit; run fetch-prep.py on Linux first")
shutil.copytree(kit, OUT / "prep-toolchain")
for name in ("windows-runner", "windows-vm", "hyperv-spike"):
    shutil.copytree(ROOT / "script" / name, OUT / "script" / name,
                    ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
shutil.copyfile(ROOT / "script/windows-runner/README.md", OUT / "README.md")
shutil.copyfile(provenance_path, OUT / 'binary-provenance.json')
for name in ("LICENSE", "THIRD-PARTY-NOTICES.md"):
    source = ROOT / name
    if source.is_file():
        shutil.copyfile(source, OUT / name)
files = []
for path in sorted(OUT.rglob("*")):
    if path.is_symlink():
        raise SystemExit(f"Unexpected bundle symlink: {path}")
    if path.is_file():
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        files.append(dict(path=path.relative_to(OUT).as_posix(), bytes=path.stat().st_size, sha256=digest))
for artifact in provenance['artifacts']:
    path = OUT / 'bin' / Path(artifact['path']).name
    if hashlib.sha256(path.read_bytes()).hexdigest() != artifact['sha256']:
        raise SystemExit(f'Release binary differs from its build provenance: {path}')
manifest = dict(schema=1, source_commit=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(), binary_source_commit=provenance['binary_source_commit'], files=files)
(OUT / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
archive = ROOT / "target/motor-windows-release.zip"
with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as output:
    for path in sorted(OUT.rglob("*")):
        if path.is_file():
            output.write(path, path.relative_to(OUT).as_posix())
with archive.open("rb") as source:
    digest = hashlib.file_digest(source, "sha256").hexdigest()
print(f"RELEASE {archive} files={len(files)+1} bytes={archive.stat().st_size} SHA256={digest}")
