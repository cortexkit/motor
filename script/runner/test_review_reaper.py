"""Tests for review_reaper.py. Run: python3 -m unittest script/runner/test_review_reaper.py

The process table and mount table are fake directories and files, so these
tests need neither root nor Linux. ctime cannot be set backwards, so "idle"
is simulated by evaluating at a `now` past the TTL.
"""

from __future__ import annotations

import os
import stat
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
# The reaper is a sibling script, not a package, so it is importable only after
# the path insert above; the lint rule against late imports does not apply.
import review_reaper as rr  # noqa: E402

TTL = 60.0


class ReaperTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        base = Path(self._tmp.name).resolve()
        self.root = base / "review"
        self.proc = base / "proc"
        self.mountinfo = base / "mountinfo"
        self.root.mkdir()
        self.proc.mkdir()
        self.mountinfo.write_text("22 1 0:21 / / rw - xfs /dev/md5 rw\n")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def copy(self, name: str) -> Path:
        d = self.root / name
        (d / "target" / "debug").mkdir(parents=True)
        (d / "Cargo.toml").write_text("[package]\n")
        (d / "target" / "debug" / "app").write_text("bin")
        return d

    def process(self, pid: int, *, cwd: str = "/", exe: str = "/usr/bin/sh", fds=()) -> None:
        p = self.proc / str(pid)
        (p / "fd").mkdir(parents=True)
        os.symlink(cwd, p / "cwd")
        os.symlink("/", p / "root")
        os.symlink(exe, p / "exe")
        for i, target in enumerate(fds):
            os.symlink(target, p / "fd" / str(i))

    def run_reaper(self, *, idle: bool = True, dry_run: bool = False):
        now = time.time() + (TTL * 2 if idle else 0)
        decisions = rr.reap(
            self.root, now=now, ttl=TTL, proc=self.proc,
            mountinfo=self.mountinfo, dry_run=dry_run,
        )
        return {d.name: d.action for d in decisions}

    def old_directory_times(self):
        # ctime cannot be backdated. Age directories at the stat boundary while
        # preserving the files' real timestamps, inode identities and link counts.
        real_lstat = os.lstat
        old = time.time() - TTL * 2

        def lstat(path, *args, **kwargs):
            st = real_lstat(path, *args, **kwargs)
            if stat.S_ISDIR(st.st_mode):
                values = list(st)
                values[8:10] = (old, old)
                return os.stat_result(values)
            return st

        return mock.patch.object(rr.os, "lstat", side_effect=lstat)

    def test_idle_unused_copy_and_loose_file_are_deleted(self) -> None:
        self.copy("alf-gate")
        (self.root / "bg78.bundle").write_text("bundle")
        self.assertEqual(self.run_reaper(), {"alf-gate": "reap", "bg78.bundle": "reap"})
        self.assertEqual(list(self.root.iterdir()), [])

    def test_fresh_copy_is_kept(self) -> None:
        self.copy("alf-gate")
        self.assertEqual(self.run_reaper(idle=False), {"alf-gate": "active"})
        self.assertTrue((self.root / "alf-gate" / "Cargo.toml").exists())

    def test_old_mtime_with_new_ctime_counts_as_written(self) -> None:
        # rsync -a and git checkout can give a fresh copy year-old mtimes. Its
        # ctime is still the creation time, so the copy must count as recent.
        d = self.copy("rsynced")
        year_ago = time.time() - 365 * 86400
        for path in [d, *d.rglob("*")]:
            os.utime(path, (year_ago, year_ago), follow_symlinks=False)
        self.assertEqual(self.run_reaper(idle=False), {"rsynced": "active"})

    def test_idle_hardlinked_copy_and_loose_file_ignore_external_ctime(self) -> None:
        d = self.root / "hardlinked"
        (d / "target").mkdir(parents=True)
        other = Path(self._tmp.name) / "other"
        other.mkdir()
        old = time.time() - TTL * 2
        links = {
            "Cargo.toml": d / "Cargo.toml",
            "app": d / "target" / "app",
            "bundle": self.root / "shared.bundle",
        }
        for name, destination in links.items():
            source = other / name
            source.write_text(name)
            os.utime(source, (old, old))
            os.link(source, destination)
            # Linking through the other tree refreshes the shared inode's ctime,
            # without changing this copy's entries or file contents.
            os.link(source, other / f"{name}.new")
            st = os.lstat(destination)
            self.assertEqual(st.st_nlink, 3)
            self.assertEqual(st.st_ino, os.lstat(source).st_ino)
            self.assertLess(st.st_mtime, time.time() - TTL)
            self.assertGreater(st.st_ctime, time.time() - TTL)

        with self.old_directory_times():
            self.assertEqual(self.run_reaper(idle=False), {
                "hardlinked": "reap", "shared.bundle": "reap",
            })
        self.assertEqual(list(self.root.iterdir()), [])
        for name in links:
            self.assertEqual((other / name).read_text(), name)

    def test_fresh_hardlink_copy_with_old_mtimes_is_kept(self) -> None:
        d = self.root / "fresh-hardlinks"
        (d / "target").mkdir(parents=True)
        source = Path(self._tmp.name) / "source"
        source.write_text("bin")
        old = time.time() - TTL * 2
        os.utime(source, (old, old))
        destination = d / "target" / "app"
        os.link(source, destination)
        self.assertEqual(os.lstat(destination).st_nlink, 2)
        # Preserved directory mtimes still leave local directory ctime fresh.
        for directory in [d, d / "target"]:
            os.utime(directory, (old, old))
        self.assertEqual(self.run_reaper(idle=False), {"fresh-hardlinks": "active"})
        self.assertTrue(destination.exists())

    def test_single_link_file_with_old_mtime_and_fresh_ctime_is_kept(self) -> None:
        d = self.root / "single-link"
        d.mkdir()
        path = d / "Cargo.toml"
        path.write_text("[package]\n")
        old = time.time() - TTL * 2
        os.utime(path, (old, old))
        self.assertEqual(os.lstat(path).st_nlink, 1)
        with self.old_directory_times():
            self.assertEqual(self.run_reaper(idle=False), {"single-link": "active"})
        self.assertTrue(path.exists())

    def test_hardlinked_file_with_fresh_mtime_is_kept(self) -> None:
        d = self.root / "written-hardlink"
        d.mkdir()
        source = Path(self._tmp.name) / "source"
        source.write_text("bin")
        destination = d / "app"
        os.link(source, destination)
        self.assertEqual(os.lstat(destination).st_nlink, 2)
        with self.old_directory_times():
            self.assertEqual(self.run_reaper(idle=False), {"written-hardlink": "active"})
        self.assertTrue(destination.exists())

    def test_one_recent_deep_file_keeps_the_whole_copy(self) -> None:
        d = self.copy("deep")
        recent = os.lstat(d / "target" / "debug" / "app").st_ino
        now = time.time() + TTL * 2
        with mock.patch.object(rr, "_stamp", lambda st: now if st.st_ino == recent else 0.0):
            self.assertEqual(self.run_reaper(), {"deep": "active"})
        self.assertTrue((d / "target" / "debug" / "app").exists())

    def test_cwd_open_file_or_running_binary_keeps_copy(self) -> None:
        for name, kwargs in {
            "by-cwd": {"cwd": "{d}"},
            "by-fd": {"fds": ["{d}/target/debug/app"]},
            "by-exe": {"exe": "{d}/target/debug/app (deleted)"},
        }.items():
            with self.subTest(name):
                d = self.copy(name)
                pid = 100 + len(list(self.proc.iterdir()))
                self.process(pid, **{
                    k: (v.format(d=d) if isinstance(v, str) else [x.format(d=d) for x in v])
                    for k, v in kwargs.items()
                })
                self.assertEqual(self.run_reaper()[name], "in-use")
                self.assertTrue((d / "Cargo.toml").exists())

    def test_sibling_with_shared_prefix_does_not_protect(self) -> None:
        self.copy("main")
        runner = self.copy("main-runner")
        self.process(200, cwd=str(runner))
        self.assertEqual(self.run_reaper(), {"main": "reap", "main-runner": "in-use"})

    def test_symlink_is_skipped_and_target_untouched(self) -> None:
        outside = Path(self._tmp.name) / "src" / "subconscious"
        outside.mkdir(parents=True)
        (outside / "keep").write_text("x")
        os.symlink(outside, self.root / "subconscious")
        self.assertEqual(self.run_reaper(), {"subconscious": "skip"})
        self.assertTrue((outside / "keep").exists())
        self.assertTrue((self.root / "subconscious").is_symlink())

    def test_entry_with_mount_point_inside_is_skipped(self) -> None:
        d = self.copy("with mount")
        escaped = str(d / "target").replace(" ", "\\040")
        with self.mountinfo.open("a") as f:
            f.write(f"40 22 0:50 / {escaped} rw - tmpfs tmpfs rw\n")
        self.assertEqual(self.run_reaper(), {"with mount": "skip"})
        self.assertTrue((d / "Cargo.toml").exists())

    def test_process_arriving_after_first_check_restores_copy(self) -> None:
        d = self.copy("raced")
        answers = iter([False, True])
        with mock.patch.object(rr, "in_use", lambda path, proc: next(answers)):
            self.assertEqual(self.run_reaper(), {"raced": "in-use"})
        self.assertTrue((d / "Cargo.toml").exists())
        self.assertFalse((self.root / ".reaping-raced").exists())

    def test_leftover_from_interrupted_run_is_finished_even_if_recent(self) -> None:
        self.copy(".reaping-old")
        self.assertEqual(self.run_reaper(idle=False), {".reaping-old": "reap"})
        self.assertFalse((self.root / ".reaping-old").exists())

    def test_dry_run_deletes_nothing(self) -> None:
        d = self.copy("alf-gate")
        self.assertEqual(self.run_reaper(dry_run=True), {"alf-gate": "reap"})
        self.assertTrue((d / "Cargo.toml").exists())

    def test_unreadable_process_aborts_without_deleting(self) -> None:
        d = self.copy("alf-gate")
        self.process(300)
        with mock.patch.object(rr.os, "readlink", side_effect=PermissionError("fd")):
            with self.assertRaises(PermissionError):
                self.run_reaper()
        self.assertTrue((d / "Cargo.toml").exists())

    def test_unlistable_directory_aborts_without_deleting(self) -> None:
        d = self.copy("alf-gate")
        real_scandir = os.scandir

        def scandir(path):
            if str(path).endswith("target"):
                raise PermissionError(path)
            return real_scandir(path)

        with mock.patch.object(rr.os, "scandir", scandir):
            with self.assertRaises(PermissionError):
                self.run_reaper()
        self.assertTrue((d / "Cargo.toml").exists())


if __name__ == "__main__":
    unittest.main()
