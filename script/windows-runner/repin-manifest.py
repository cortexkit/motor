#!/usr/bin/env python3
"""Advance VS metadata only after reproducing the qualified private SDK tree."""
import argparse
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
PIN_PATH = Path("script/runner/toolchain.json")


def fetch_candidate(pins):
    url = f"https://aka.ms/vs/{pins['vs_manifest']}/release/channel"
    with urllib.request.urlopen(url, timeout=60) as response:
        channel = json.load(response)
    items = [item for item in channel["channelItems"]
             if item["id"] == "Microsoft.VisualStudio.Manifests.VisualStudio"]
    if len(items) != 1 or len(items[0]["payloads"]) != 1:
        raise ValueError("ambiguous Microsoft package manifest")
    with urllib.request.urlopen(items[0]["payloads"][0]["url"], timeout=60) as response:
        return response.read()


def check_components(pins, package):
    differences = []
    for name, expected in pins["resolved_components"].items():
        versions = sorted({item["version"] for item in package["packages"]
                           if item["id"] == name})
        if versions != [expected]:
            differences.append(f"{name}: expected {expected}, got {versions or 'missing'}")
    if differences:
        raise ValueError("resolved Microsoft components changed:\n" + "\n".join(differences))


def build_cache(root, candidate):
    # A fresh staging root prevents a previous cache's DONE marker from hiding
    # changed downloads, and keeps provisional pins out of the real toolchain file.
    scratch = root / "target/scratch"
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="repin-manifest-", dir=scratch) as temporary:
        stage = Path(temporary)
        (stage / PIN_PATH).parent.mkdir(parents=True)
        (stage / PIN_PATH).write_text(json.dumps(candidate, indent=2) + "\n")
        scripts = stage / "script/windows-runner"
        scripts.mkdir(parents=True)
        for name in ("install-cross.sh", "fetch-manifest.py", "cache-manifest.py"):
            shutil.copyfile(root / "script/windows-runner" / name, scripts / name)
        result = subprocess.run(
            ["bash", str(scripts / "install-cross.sh"), "--accept-microsoft-terms"],
            cwd=stage, env={**os.environ, "CARGO_NET_OFFLINE": "false"}, check=False,
        )
        pins = candidate["windows_cross"]
        cache = stage / pins["cache"]
        spec = importlib.util.spec_from_file_location("cache_manifest", scripts / "cache-manifest.py")
        if spec is None or spec.loader is None:
            raise ValueError("cannot load SDK tree verifier")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        if result.returncode:
            # The installer also enforces the tree pin. Report the measured hash
            # when available, but never turn an unrelated install failure into success.
            if (cache / pins["sdk_subdir"]).is_dir():
                actual = module.manifest(cache, pins["sdk_subdir"])["tree_sha256"]
                check_tree(pins, actual)
            raise ValueError(f"cross-cache installation failed (exit {result.returncode}); pins unchanged")
        return module.manifest(cache, pins["sdk_subdir"])["tree_sha256"]


def check_tree(pins, actual):
    if actual != pins["cache_tree_sha256"]:
        raise ValueError(f"SDK tree changed: expected {pins['cache_tree_sha256']}, got {actual}")


def repin(root, fetch=fetch_candidate, build=build_cache):
    path = root / PIN_PATH
    original = path.read_text()
    config = json.loads(original)
    pins = config["windows_cross"]
    package_bytes = fetch(pins)
    package = json.loads(package_bytes)
    check_components(pins, package)
    moved = {
        "package_manifest_sha256": hashlib.sha256(package_bytes).hexdigest(),
        "vs_channel_build": package["info"]["buildVersion"],
    }
    candidate = copy.deepcopy(config)
    candidate["windows_cross"].update(moved)
    actual = build(root, candidate)
    check_tree(pins, actual)
    updated = original
    for key, value in moved.items():
        old = f'"{key}": {json.dumps(pins[key])}'
        if updated.count(old) != 1:
            raise ValueError(f"cannot safely replace {key}; pins unchanged")
        updated = updated.replace(old, f'"{key}": {json.dumps(value)}', 1)
    if json.loads(updated) != candidate or path.read_text() != original:
        raise ValueError("toolchain file changed during verification; pins unchanged")
    # Replace atomically only after both immutable-content checks have succeeded.
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(updated)
            output.flush()
            os.fchmod(output.fileno(), path.stat().st_mode & 0o777)
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)
    for key, value in moved.items():
        print(f"{key}: {pins[key]} -> {value}")
    print(f"Unchanged resolved_components: {json.dumps(pins['resolved_components'], sort_keys=True)}")
    print(f"Unchanged SDK tree SHA256={actual}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-microsoft-terms", action="store_true",
                        help="approve private Microsoft Build Tools/SDK downloads")
    args = parser.parse_args()
    if not args.accept_microsoft_terms:
        parser.error("pass --accept-microsoft-terms only after approving the Microsoft Build Tools/SDK terms")
    try:
        repin(ROOT)
    except (ValueError, OSError, KeyError) as error:
        raise SystemExit(f"FAIL {error}") from error


if __name__ == "__main__":
    main()
