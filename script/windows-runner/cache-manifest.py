#!/usr/bin/env python3
"""Record the private SDK tree's relative names and byte hashes, never SDK bytes."""
import hashlib
import json
from pathlib import Path
import sys


def manifest(root):
    files = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            files.append({"path": path.relative_to(root).as_posix(), "symlink": str(path.readlink())})
        elif path.is_file():
            with path.open("rb") as source:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
            files.append({"path": path.relative_to(root).as_posix(), "sha256": digest, "bytes": path.stat().st_size})
    if not files:
        raise ValueError("SDK cache is empty")
    encoded = json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    return {"tree_sha256": hashlib.sha256(encoded).hexdigest(), "file_count": len(files), "files": files}


if __name__ == "__main__":
    data = manifest(Path(sys.argv[1]))
    if len(sys.argv) > 3:
        pins = json.loads(Path(sys.argv[3]).read_text())["windows_cross"]
        root = Path(sys.argv[1])
        matching = [root.parent / "provenance/manifest.vsman"]
        if not matching[0].is_file() or hashlib.sha256(matching[0].read_bytes()).hexdigest() != pins["package_manifest_sha256"]:
            raise SystemExit("FAIL Microsoft package manifest content changed; explicit pin review required")
        package_manifest = json.loads(matching[0].read_text())
        if package_manifest["info"]["buildVersion"] != pins["vs_channel_build"]:
            raise SystemExit("FAIL resolved VS channel version changed")
        packages = {p["id"]: p["version"] for p in package_manifest["packages"]}
        for name, version in pins["resolved_components"].items():
            if packages.get(name) != version:
                raise SystemExit(f"FAIL resolved Microsoft component changed: {name}")
        if pins.get("cache_tree_sha256") and data["tree_sha256"] != pins["cache_tree_sha256"]:
            raise SystemExit("FAIL SDK cache content differs from the qualified tree hash")
        print(f"VS manifest build={pins['vs_channel_build']} sha256={pins['package_manifest_sha256']}; components={pins['resolved_components']}")
    output = Path(sys.argv[2])
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(data, indent=2) + "\n")
    print(f"SDK cache: {data['file_count']} entries tree_sha256={data['tree_sha256']}; manifest={output}")
