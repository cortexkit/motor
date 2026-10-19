#!/usr/bin/env python3
"""Portable mount-fence and allocated-storage accounting tests; no real mounts."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import build_storage as storage

ROOT = "1 0 8:1 / / rw - xfs /dev/root rw\n"


def row(identifier, path):
    escaped = str(path).replace("\\", "\\134").replace(" ", "\\040")
    return f"{identifier} 1 0:2 / {escaped} rw - tmpfs stub rw\n"


class BuildStorageTests(unittest.TestCase):
    def test_mount_fence_decodes_paths_and_does_not_match_sibling_prefix(self):
        text = ROOT + row(2, "/stage space/root/dev") + row(3, "/stage space-old/root/dev")
        self.assertEqual(storage.mounts_under(Path("/stage space"), text), [(2, Path("/stage space/root/dev"))])
        with self.assertRaisesRegex(RuntimeError, "/stage space/root/dev"):
            storage.assert_unmounted(Path("/stage space"), lambda: text)
        storage.assert_unmounted(Path("/other"), lambda: text)

    def test_empty_or_malformed_mountinfo_never_means_unmounted(self):
        for text in ("", "broken", "1 0 8:1 / relative rw - xfs /dev/root rw\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                storage.assert_unmounted(Path("/stage"), lambda: text)

    def test_blocking_unmounts_are_deepest_first_and_remove_stacked_mounts(self):
        entries = [(2, Path("/stage/root/dev")), (3, Path("/stage/root/dev/pts")),
                   (4, Path("/stage/root/dev/shm/deep")), (5, Path("/stage/root/dev"))]
        commands = []
        def read():
            return ROOT + "".join(row(*entry) for entry in entries)
        def execute(argv, **kwargs):
            commands.append(argv)
            self.assertEqual(argv[:2], ["umount", "--"])
            self.assertTrue(kwargs["check"])
            matches = [entry for entry in entries if entry[1] == Path(argv[-1])]
            entries.remove(max(matches))
        with mock.patch("builtins.print"):
            storage.unmount_tree(Path("/stage"), read, execute)
        self.assertEqual(entries, [])
        self.assertEqual(commands[0][-1], "/stage/root/dev/shm/deep")
        self.assertEqual(commands[1][-1], "/stage/root/dev/pts")
        self.assertEqual(len(commands), 4)

    def test_busy_or_unchanged_mount_is_named_and_never_silently_detached(self):
        text = ROOT + row(2, "/stage/root/dev")
        error = subprocess.CalledProcessError(32, ["umount"], stderr="target is busy")
        execute = mock.Mock(side_effect=error)
        with self.assertRaisesRegex(RuntimeError, "/stage/root/dev.*target is busy"):
            storage.unmount_tree(Path("/stage"), lambda: text, execute)
        self.assertEqual(execute.call_count, 1)
        with self.assertRaisesRegex(RuntimeError, "mount remains.*dev"):
            storage.unmount_tree(Path("/stage"), lambda: text, mock.Mock())

    def test_allocated_bytes_ignore_sparse_holes_links_and_mounted_subtrees(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            version, external = root / "version", root / "external"
            version.mkdir()
            external.write_bytes(b"x" * 1024**2)
            sparse = version / "sparse"
            with sparse.open("wb") as out:
                out.truncate(32 * storage.GIB)
            data = version / "data"
            data.write_bytes(b"real data" * 512)
            os.link(data, version / "hardlink")
            (version / "symlink").symlink_to(external)
            mounted = version / "root/dev"
            mounted.mkdir(parents=True)
            (mounted / "not-owned").write_bytes(b"y" * 1024**2)
            text = ROOT + row(2, mounted)
            expected = sum(path.lstat().st_blocks * 512 for path in (version, version / "root", sparse, data, version / "symlink"))
            self.assertEqual(storage.allocated_bytes(version, lambda: text), expected)
            self.assertLess(expected, storage.GIB)
            with self.assertRaisesRegex(RuntimeError, "version root"):
                storage.allocated_bytes(version, lambda: ROOT + row(3, version))

    def test_first_budget_is_explicit_and_profiles_need_matching_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            profile = Path(tmp) / "disk-usage.json"
            data = dict(schema=1, outcome="success", recipe_digest="recipe", peak_allocated_bytes=10 * storage.GIB,
                        sample_count=5, measurement_errors=[])
            profile.write_text(json.dumps(data))
            self.assertEqual(storage.choose_budget(None, profile, "recipe")[0], 17 * storage.GIB)
            self.assertEqual(storage.choose_budget(64 * storage.GIB, None, "recipe")[0], 64 * storage.GIB)
            for explicit, path in ((None, None), (1, profile), (0, None), (True, None)):
                with self.subTest(explicit=explicit, path=path), self.assertRaises(ValueError):
                    storage.choose_budget(explicit, path, "recipe")
            for change in (dict(outcome="failed"), dict(recipe_digest="other"), dict(sample_count=0), dict(measurement_errors=["unreadable"])):
                profile.write_text(json.dumps(dict(data, **change)))
                with self.subTest(change=change), self.assertRaises(ValueError):
                    storage.choose_budget(None, profile, "recipe")

    def test_sampled_peak_survives_deletion_and_failure_and_is_not_global_df(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            meter = storage.DiskMeter(target, storage.GIB, "recipe", floor=0, reader=lambda: ROOT,
                                      usage=lambda _: type("Usage", (), dict(free=1000 * storage.GIB)))
            path = target / "large"
            path.write_bytes(b"a" * 1024**2)
            meter.sample()
            peak = meter.peak
            path.unlink()
            with mock.patch("builtins.print"):
                data = meter.finish("failed")
            self.assertGreaterEqual(data["peak_allocated_bytes"], peak)
            self.assertGreaterEqual(peak, 1024**2)
            self.assertLess(peak, storage.GIB)
            self.assertEqual(data["minimum_free_bytes"], 1000 * storage.GIB)
            self.assertEqual(json.loads((target / "disk-usage.json").read_text())["outcome"], "failed")

    def test_floor_refusal_precedes_command_launch(self):
        with tempfile.TemporaryDirectory() as tmp:
            meter = storage.DiskMeter(Path(tmp), storage.GIB, "recipe", floor=2, reader=lambda: ROOT,
                                      usage=lambda _: type("Usage", (), dict(free=1)))
            with mock.patch("builtins.print"), self.assertRaisesRegex(storage.DiskBudgetError, "floor"):
                meter.start()
            with mock.patch.object(storage.subprocess, "Popen") as spawn:
                with self.assertRaises(storage.DiskBudgetError):
                    meter.run(["should-not-run"])
                spawn.assert_not_called()

    def test_runtime_cap_stops_only_its_own_child_process_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            meter = storage.DiskMeter(target, 64 * 1024, "recipe", floor=0, interval=.01, reader=lambda: ROOT,
                                      usage=lambda _: type("Usage", (), dict(free=1000 * storage.GIB)))
            child = ("import os,time; from pathlib import Path; "
                     f"Path({str(target / 'pid')!r}).write_text(str(os.getpid())); "
                     f"Path({str(target / 'data')!r}).write_bytes(b'x'*1048576); time.sleep(30)")
            with mock.patch("builtins.print"):
                meter.start()
                try:
                    started = time.monotonic()
                    with self.assertRaisesRegex(storage.DiskBudgetError, "exceed"):
                        meter.run([sys.executable, "-c", child])
                    self.assertLess(time.monotonic() - started, 5, "cap must interrupt the command, not wait for its natural exit")
                    with self.assertRaises(storage.DiskBudgetError):
                        meter.finish("failed")
                finally:
                    meter.stop()
            pid = int((target / "pid").read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            self.assertGreater(json.loads((target / "disk-usage.json").read_text())["peak_allocated_bytes"], 64 * 1024)


if __name__ == "__main__":
    unittest.main(verbosity=2)
