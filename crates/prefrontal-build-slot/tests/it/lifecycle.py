"""Real process/lock tests, including descendants that keep compiler stdio open."""
import fcntl
import os
from pathlib import Path
import signal
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[4]))
from scripts.tests.test_build_budget import BuildBudgetTests


class LifecycleTests(BuildBudgetTests):
    def tearDown(self):
        # SIGKILL on the launcher leaves its compiler running because a killed
        # launcher cannot forward signals or reap children. Reap matching fake
        # compilers during teardown so they do not outlive this test.
        processes = subprocess.check_output(["ps", "-axo", "pid=,command="], text=True)
        prefix = f"{self.compiler} --emit=link {self.root}/"
        for line in processes.splitlines():
            pid, command = line.strip().split(None, 1)
            if command.startswith(prefix):
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass
        super().tearDown()

    def test_background_descendant_cannot_pin_slot(self):
        (self.root / "limit").write_text("1")
        pidfile = self.root / "background.pid"
        child = subprocess.Popen(
            [str(self.wrapper), "/bin/sh", "-c",
             'sleep 60 & echo $! > "$1"; exit 0', "--emit=link", str(pidfile)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.children.append(child)
        try:
            # wait(), not communicate(): the background process deliberately
            # keeps both pipes open, just like a detached cache server can.
            self.assertEqual(child.wait(timeout=10), 0)
            background = int(pidfile.read_text())
            os.kill(background, 0)
            with open(self.root / "0.lock", "a+") as slot:
                try:
                    fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    self.fail("slot stayed pinned by background descendant after compiler exit")
            os.kill(background, 0)
        finally:
            if pidfile.exists():
                try:
                    os.kill(int(pidfile.read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if child.stdout is not None:
                child.stdout.close()
            if child.stderr is not None:
                child.stderr.close()

    def test_compile_exit_code_is_preserved(self):
        child = subprocess.run(
            [str(self.wrapper), "/bin/sh", "-c", "exit 37", "--emit=link"], timeout=10
        )
        self.assertEqual(child.returncode, 37)

    def test_cache_path_trims_surrounding_whitespace(self):
        program = self.root / "cache"
        called = self.root / "cache.called"
        program.write_text(f'#!/bin/sh\necho called > "{called}"\nexec "$@"\n')
        program.chmod(0o700)
        cache = self.wrapper.with_suffix(".cache")
        cache.write_text(f" \t{program}\r\n ")
        result = subprocess.run(
            [str(self.wrapper), str(self.compiler), "-vV"],
            capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(called.read_text().strip(), "called")
        self.assertEqual(result.stdout.strip(), "probe completed")
        # Whitespace alone is still an empty configuration, not a wrapper name.
        cache.write_text(" \t\r\n ")
        called.unlink()
        result = subprocess.run(
            [str(self.wrapper), str(self.compiler), "-vV"],
            capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "probe completed")
        self.assertFalse(called.exists())

    def test_signal_reaches_child_and_launcher_reaps_it(self):
        pidfile = self.root / "child.pid"
        reached = self.root / "signal.reached"
        child = subprocess.Popen([
            str(self.wrapper), "/bin/sh", "-c",
            'trap \'echo reached > "$2"; exit 42\' TERM; '
            'echo $$ > "$1"; while :; do sleep .05; done',
            "--emit=link", str(pidfile), str(reached),
        ])
        self.children.append(child)
        self.await_file("child.pid")
        compiler_pid = int(pidfile.read_text())
        try:
            child.terminate()
            self.assertEqual(child.wait(timeout=10), 42)
            self.assertEqual(reached.read_text().strip(), "reached")
            with self.assertRaises(ProcessLookupError):
                os.kill(compiler_pid, 0)
        finally:
            try:
                os.kill(compiler_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def test_ignored_signal_cannot_leave_child_running(self):
        pidfile = self.root / "child.pid"
        child = subprocess.Popen([
            str(self.wrapper), "/bin/sh", "-c",
            'trap "" TERM; echo $$ > "$1"; exec sleep 60',
            "--emit=link", str(pidfile),
        ])
        self.children.append(child)
        self.await_file("child.pid")
        compiler_pid = int(pidfile.read_text())
        try:
            child.terminate()
            self.assertEqual(child.wait(timeout=15), -signal.SIGKILL)
            with self.assertRaises(ProcessLookupError):
                os.kill(compiler_pid, 0)
        finally:
            try:
                os.kill(compiler_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def test_waiter_reads_cache_after_acquiring_slot(self):
        (self.root / "limit").write_text("1")
        chosen = self.root / "chosen"
        for name in ["old", "new"]:
            program = self.root / name
            program.write_text(f'#!/bin/sh\necho {name} > "{chosen}"\nexec "$@"\n')
            program.chmod(0o700)
        cache = self.wrapper.with_suffix(".cache")
        cache.write_text(str(self.root / "old"))
        with open(self.root / "0.lock", "a+") as slot:
            fcntl.flock(slot, fcntl.LOCK_EX)
            with open(self.root / "waiter.stderr", "wb") as stderr:
                child = subprocess.Popen(
                    [str(self.wrapper), "/bin/sh", "-c", "exit 0", "--emit=link"],
                    stderr=stderr,
                    env=dict(os.environ, PREFRONTAL_BUILD_SLOT_NOTICE_AFTER_MS="20",
                             PREFRONTAL_BUILD_SLOT_NOTICE_EVERY_MS="60000"),
                )
            self.children.append(child)
            # A notice proves the launcher reached the occupied pool before
            # changing configuration, rather than relying on scheduling sleeps.
            self.await_stderr_lines("waiter", 1)
            cache.write_text(str(self.root / "new"))
            fcntl.flock(slot, fcntl.LOCK_UN)
        self.assertEqual(child.wait(timeout=10), 0)
        self.assertEqual(chosen.read_text().strip(), "new")


if __name__ == "__main__":
    if sys.argv[1:] == ["original"]:
        names = unittest.defaultTestLoader.getTestCaseNames(BuildBudgetTests)
        suite = unittest.TestSuite(LifecycleTests(name) for name in names)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        sys.exit(not result.wasSuccessful())
    unittest.main(verbosity=2)
