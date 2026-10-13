"""Exercise the installed wrapper with real exec, flock, and SIGKILL."""
import fcntl
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

SOURCE = Path(__file__).resolve().parents[2] / "crates/prefrontal-build-slot/src/main.rs"


class BuildBudgetTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binaries = tempfile.TemporaryDirectory()
        cls.native_wrapper = Path(cls.binaries.name) / 'prefrontal-build-slot'
        supplied = os.environ.get('BUILD_SLOT_BINARY')
        if supplied:
            shutil.copyfile(supplied, cls.native_wrapper)
            cls.native_wrapper.chmod(0o700)
        else:
            subprocess.run(['rustc', '--edition=2021', str(SOURCE), '-o', str(cls.native_wrapper)], check=True)
        source = Path(cls.binaries.name) / 'compiler.rs'
        source.write_text('''fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--probe-env") {
        println!("{}|{}", std::env::var("DYLD_FALLBACK_LIBRARY_PATH").unwrap_or_default(),
            std::env::var("BUILD_SLOT_SENTINEL").unwrap_or_default());
    } else if !args.iter().any(|a| a == "--emit=link") || args.iter().any(|a| a == "--print=file-names") {
        println!("probe completed");
    } else {
        std::fs::write(args.last().unwrap(), "started").unwrap();
        loop { std::thread::sleep(std::time::Duration::from_millis(10)); }
    }
}''')
        cls.native_compiler = source.with_suffix('')
        subprocess.run(['rustc', str(source), '-o', str(cls.native_compiler)], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.binaries.cleanup()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.wrapper = self.root / "prefrontal-build-slot"
        shutil.copyfile(self.native_wrapper, self.wrapper)
        self.wrapper.chmod(0o700)
        self.wrapper.with_suffix('.root').write_text(str(self.root))
        self.wrapper.with_suffix('.cache').write_text('')
        (self.root / "limit").write_text("2")
        self.compiler = self.native_compiler
        self.children = []

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.wait()
        self.tmp.cleanup()

    def launch(self, name, notice_after_ms=None, notice_every_ms=None):
        worktree = self.root / ('worktree-a' if name == 'first' else 'worktree-b')
        worktree.mkdir(exist_ok=True)
        env = dict(os.environ)
        for variable, value in [('PREFRONTAL_BUILD_SLOT_NOTICE_AFTER_MS', notice_after_ms),
                                ('PREFRONTAL_BUILD_SLOT_NOTICE_EVERY_MS', notice_every_ms)]:
            env.pop(variable, None)
            if value is not None:
                env[variable] = str(value)
        # Each compile's stderr goes to its own file so a test can read what it printed.
        with open(self.root / (name + '.stderr'), 'wb') as stderr:
            child = subprocess.Popen([str(self.wrapper), str(self.compiler),
                                      '--emit=link', str(self.root / name)], cwd=worktree,
                                     env=env, stderr=stderr)
        self.children.append(child)
        return child

    def await_file(self, name):
        deadline = time.monotonic() + 5
        while not (self.root / name).exists():
            if time.monotonic() > deadline:
                self.fail('compiler did not acquire a slot: ' + name)
            time.sleep(.01)

    def test_shared_limit_and_sigkill_recovery(self):
        first = self.launch('first')
        self.await_file('first')
        second = self.launch('second')
        self.await_file('second')
        third = self.launch('third')
        time.sleep(.25)
        self.assertFalse((self.root / 'third').exists(), 'third compiler bypassed the two-slot limit')
        first.kill()
        first.wait()
        self.await_file('third')
        second.kill()
        third.kill()
        second.wait()
        third.wait()
        # All slots return after death without deleting or replacing lock files.
        self.launch('fourth')
        self.launch('fifth')
        self.await_file('fourth')
        self.await_file('fifth')

    def test_waiters_queue_on_one_blocking_lock_that_the_compiler_does_not_keep(self):
        # Only the head of the queue polls the slots; everyone else blocks in the kernel.
        # Holding the queue lock here stands in for an earlier waiter at the head.
        queue = open(self.root / 'queue.lock', 'a+')
        fcntl.flock(queue, fcntl.LOCK_EX)
        self.launch('first')
        time.sleep(.25)
        self.assertFalse((self.root / 'first').exists(), 'a waiter took a free slot without queueing')
        fcntl.flock(queue, fcntl.LOCK_UN)
        self.await_file('first')
        # The running compiler must not inherit the queue lock, or the queue stalls behind it.
        fcntl.flock(queue, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(queue, fcntl.LOCK_UN)
        queue.close()
        self.launch('second')
        self.await_file('second')

    def stderr_lines(self, name):
        return (self.root / (name + '.stderr')).read_text().splitlines()

    def await_stderr_lines(self, name, count):
        deadline = time.monotonic() + 5
        while len(self.stderr_lines(name)) < count:
            if time.monotonic() > deadline:
                self.fail(f'{name} printed {self.stderr_lines(name)!r}, expected {count} lines')
            time.sleep(.01)
        return self.stderr_lines(name)

    def test_waiting_compile_says_it_is_not_hung_after_the_threshold(self):
        first = self.launch('first')
        self.launch('second')
        self.await_file('first')
        self.await_file('second')
        launched = time.monotonic()
        self.launch('third', notice_after_ms=1000, notice_every_ms=400)
        time.sleep(.6)
        self.assertEqual(self.stderr_lines('third'), [], 'notice printed before the threshold')
        lines = self.await_stderr_lines('third', 1)
        self.assertGreaterEqual(time.monotonic() - launched, 1.0)
        self.assertEqual(lines[0], "prefrontal-build-slot: waiting for a compile slot (2 slots, all busy; "
                                   "queued behind other worktrees' builds). This is not a hang.")
        lines = self.await_stderr_lines('third', 2)
        self.assertRegex(lines[1], r"^prefrontal-build-slot: still waiting for a compile slot after \d+ s "
                                   r"\(2 slots, all busy; queued behind other worktrees' builds\)\. "
                                   r"This is not a hang\.$")
        first.kill()
        first.wait()
        self.await_file('third')
        # Once the compiler holds its slot the notices stop.
        printed = self.stderr_lines('third')
        time.sleep(1)
        self.assertEqual(self.stderr_lines('third'), printed)

    def test_compile_that_gets_a_slot_within_the_threshold_prints_nothing(self):
        # A free slot: nothing at all, under the default one-minute threshold.
        self.launch('first')
        self.await_file('first')
        second = self.launch('second', notice_after_ms=1500)
        self.await_file('second')
        # Every slot busy, but one frees up before the threshold: still nothing, even after
        # the threshold has passed while the compiler runs.
        self.launch('third', notice_after_ms=1500)
        time.sleep(.3)
        second.kill()
        second.wait()
        self.await_file('third')
        time.sleep(1.6)
        self.assertEqual(self.stderr_lines('first'), [])
        self.assertEqual(self.stderr_lines('third'), [])

    def test_cargo_queries_bypass_occupied_slots(self):
        self.launch('first')
        self.launch('second')
        self.await_file('first')
        self.await_file('second')
        for arguments in [['-vV'], ['--version'], ['-V'], ['--emit=link', '--print=file-names']]:
            result = subprocess.run([str(self.wrapper), str(self.compiler), *arguments], capture_output=True,
                                    text=True, timeout=2)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('probe completed', result.stdout)

    def test_cache_wrapper_receives_original_compiler_and_arguments(self):
        cache = self.root / 'cache'
        cache.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "' + str(self.root / 'cache-args') + '"\nexec "$@"\n')
        cache.chmod(0o700)
        self.wrapper.with_suffix('.cache').write_text(str(cache))
        result = subprocess.run([str(self.wrapper), str(self.compiler), '-vV'], capture_output=True, text=True, timeout=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'cache-args').read_text().splitlines(),
                          [str(self.compiler), '-vV'])

    def test_exit_status_and_signal_are_passthrough(self):
        for command, expected in [('exit 37', 37), ('kill -TERM $$', -15)]:
            result = subprocess.run([str(self.wrapper), '/bin/sh', '-c', command], timeout=2)
            self.assertEqual(result.returncode, expected)

    def test_installed_binary_preserves_cargo_dyld_environment(self):
        env = dict(os.environ, DYLD_FALLBACK_LIBRARY_PATH=str(self.root),
                   BUILD_SLOT_SENTINEL='preserved')
        result = subprocess.run([str(self.wrapper), str(self.compiler), '--emit=link', '--probe-env'],
                                env=env, capture_output=True, text=True, timeout=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.root) + '|preserved')


if __name__ == '__main__':
    unittest.main()
