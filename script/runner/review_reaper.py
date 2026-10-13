#!/usr/bin/env python3
"""Delete idle review copies under /srv/build/review on the build server.

Review copies keep source trees and Cargo targets under /srv/build/review/<name>/;
idle copies must be removed to preserve disk space for runner jobs.
This reaper runs hourly as root (see
review-reaper.service/.timer) and deletes a top-level entry only when both hold:

* nothing under it was written or created for TTL (6 h). Activity uses directory
  mtime and ctime, file mtime, and single-link file ctime. This detects fresh
  rsync copies with old mtimes without counting metadata changes to hardlinked
  files made through another tree.
* no process has its cwd, root, executable or an open file inside it.

Before deleting, the entry is renamed to a `.reaping-` name and the process
check repeats, so a process that started using it after the first check keeps
it (the rename is undone). Symlinks are never followed or deleted, and an entry
containing a mount point is skipped. Keep each review in a separate top-level
copy so activity and process checks protect the whole review.
"""

from __future__ import annotations

import argparse
import fcntl
import os
import re
import shutil
import stat
import sys
import time
from dataclasses import dataclass
from pathlib import Path

REVIEW_ROOT = Path("/srv/build/review")
TTL_SECONDS = 6 * 3600
LOCK_PATH = Path("/run/motor-review-reaper/lock")
REAPING_PREFIX = ".reaping-"


@dataclass(frozen=True)
class Decision:
    name: str
    action: str  # "reap", "active", "in-use", "skip"
    reason: str


def _stamp(st: os.stat_result) -> float:
    # A shared file's ctime can change through another link outside this copy.
    # Directory ctime remains local and detects entry changes with old mtimes.
    if stat.S_ISREG(st.st_mode) and st.st_nlink > 1:
        return st.st_mtime
    return max(st.st_mtime, st.st_ctime)


def written_since(path: Path, cutoff: float) -> bool:
    """True if anything at or below path was written or created after cutoff.

    Stops at the first recent entry, so an active copy costs little; an idle
    copy is walked in full, once per run.
    """
    st = os.lstat(path)
    if _stamp(st) > cutoff:
        return True
    if not os.path.isdir(path) or os.path.islink(path):
        return False
    def fail(err: OSError) -> None:
        # os.walk skips unreadable directories by default; a directory that
        # cannot be listed could hide a recent write, so stop instead.
        raise err

    for dirpath, dirnames, filenames in os.walk(path, onerror=fail, followlinks=False):
        for name in dirnames + filenames:
            try:
                if _stamp(os.lstat(os.path.join(dirpath, name))) > cutoff:
                    return True
            except FileNotFoundError:
                # Something is deleting files right now, which is itself
                # activity: keep the copy.
                return True
    return False


def _proc_links(proc: Path) -> list[str]:
    """Every path a process holds: cwd, root, exe and open file descriptors."""
    links: list[str] = []
    for pid_dir in proc.iterdir():
        if not pid_dir.name.isdigit():
            continue
        candidates = [pid_dir / "cwd", pid_dir / "root", pid_dir / "exe"]
        try:
            candidates.extend((pid_dir / "fd").iterdir())
        except (FileNotFoundError, ProcessLookupError):
            continue  # the process exited mid-scan
        for link in candidates:
            try:
                target = os.readlink(link)
            except (FileNotFoundError, ProcessLookupError):
                continue
            # Kernel threads have no exe; that is ENOENT above. Anything else,
            # such as EACCES, propagates: an incomplete scan must not reap.
            links.append(target.removesuffix(" (deleted)"))
    return links


def _under(target: str, path: Path) -> bool:
    p = str(path)
    return target == p or target.startswith(p + "/")


def in_use(path: Path, proc: Path) -> bool:
    return any(_under(t, path) for t in _proc_links(proc))


def _mount_points(mountinfo: Path) -> list[str]:
    points = []
    for line in mountinfo.read_text().splitlines():
        # Field 5 is the mount point; the kernel writes space, tab, newline
        # and backslash as three-digit octal escapes.
        raw = line.split(" ")[4]
        points.append(re.sub(r"\\([0-7]{3})", lambda m: chr(int(m.group(1), 8)), raw))
    return points


def has_mount(path: Path, mountinfo: Path) -> bool:
    return any(_under(m, path) for m in _mount_points(mountinfo))


def _delete(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        # On Linux rmtree uses fd-relative calls and never follows symlinks.
        shutil.rmtree(path)
    else:
        path.unlink()


def reap(
    root: Path,
    *,
    now: float,
    ttl: float = TTL_SECONDS,
    proc: Path = Path("/proc"),
    mountinfo: Path = Path("/proc/self/mountinfo"),
    dry_run: bool = False,
) -> list[Decision]:
    cutoff = now - ttl
    decisions: list[Decision] = []
    for entry in sorted(root.iterdir()):
        name = entry.name
        if entry.is_symlink():
            decisions.append(Decision(name, "skip", "symlink"))
            continue
        if has_mount(entry, mountinfo):
            decisions.append(Decision(name, "skip", "contains a mount point"))
            continue
        # A `.reaping-` entry is left by a run that died mid-delete: it already
        # passed both checks, so only the process check repeats.
        leftover = name.startswith(REAPING_PREFIX)
        if not leftover and written_since(entry, cutoff):
            decisions.append(Decision(name, "active", "written within TTL"))
            continue
        if in_use(entry, proc):
            decisions.append(Decision(name, "in-use", "a process uses it"))
            continue
        if dry_run:
            decisions.append(Decision(name, "reap", "dry run, not deleted"))
            continue
        if leftover:
            staged = entry
        else:
            staged = root / f"{REAPING_PREFIX}{name}"
            os.rename(entry, staged)
            if in_use(staged, proc):
                os.rename(staged, entry)
                decisions.append(Decision(name, "in-use", "a process started using it"))
                continue
        _delete(staged)
        decisions.append(Decision(name, "reap", "idle and unused"))
    return decisions


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dry-run", action="store_true", help="report, delete nothing")
    args = parser.parse_args(argv)
    if os.geteuid() != 0:
        # Without root, /proc hides the open files of processes owned by other
        # accounts (runner jobs run as motor-job; root-only tests run as root),
        # so the process check could miss a copy in use.
        print("review-reaper: must run as root", file=sys.stderr)
        return 2
    if not REVIEW_ROOT.is_dir():
        print(f"review-reaper: {REVIEW_ROOT} missing, nothing to do")
        return 0
    LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(LOCK_PATH, "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("review-reaper: another run holds the lock", file=sys.stderr)
            return 1
        for d in reap(REVIEW_ROOT, now=time.time(), dry_run=args.dry_run):
            print(f"{d.action:7} {d.name}: {d.reason}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
