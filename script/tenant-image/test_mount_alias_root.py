#!/usr/bin/env python3
"""Operator-only bind-alias regression check, launched in a private mount namespace."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from build_storage import assert_unmounted, mounts_under, read_mountinfo, require_private_view, unmount_tree

ROOT = Path("/var/lib/motor-vm")


def expect_refusal(stage, mounted):
    matches = mounts_under(stage, read_mountinfo())
    if mounted not in [path for _, path in matches]:
        raise RuntimeError(f"bind alias escaped the stage fence: {matches}")
    try:
        assert_unmounted(stage)
    except RuntimeError as error:
        if str(mounted) not in str(error):
            raise
        print(f"expected alias refusal: {error}", flush=True)
    else:
        raise RuntimeError("mounted bind alias was incorrectly accepted as unmounted")
    unmount_tree(stage)
    assert_unmounted(stage)


def main():
    if sys.platform != "linux" or os.geteuid() != 0:
        raise RuntimeError("real bind-alias check requires Linux root; it is not skipped")
    if os.readlink("/proc/self/ns/mnt") == os.readlink("/proc/1/ns/mnt"):
        raise RuntimeError("launch with unshare --mount --propagation private, not the host mount namespace")
    require_private_view(ROOT)
    base = Path(tempfile.mkdtemp(prefix=".mount-alias-check-", dir=ROOT))
    source, alias = base / "source", base / "alias"
    stage = source / "stage"
    mounted = alias / "stage/dev"
    (stage / "dev").mkdir(parents=True)
    alias.mkdir()
    try:
        subprocess.run(["mount", "--bind", str(source), str(alias)], check=True)
        subprocess.run(["mount", "-t", "tmpfs", "-o", "size=1m", "tmpfs", str(mounted)], check=True)
        expect_refusal(stage, mounted)
        mounted = alias / "stage"
        subprocess.run(["mount", "-t", "tmpfs", "-o", "size=1m", "tmpfs", str(mounted)], check=True)
        expect_refusal(stage, mounted)
        print("PASS real bind-alias mount fence: 2 checks passed", flush=True)
    finally:
        try:
            unmount_tree(base)
            assert_unmounted(base)
        except Exception as error:
            raise RuntimeError(f"alias-check cleanup failed; retain {base}: {error}") from error
        shutil.rmtree(base)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"FAIL real bind-alias mount fence: {type(error).__name__}: {error}", flush=True)
        raise
