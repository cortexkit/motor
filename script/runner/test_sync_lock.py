"""Run with python3 -m unittest -v script/runner/test_sync_lock.py."""
import fcntl
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import textwrap
import time
import unittest

SYNC = Path(__file__).with_name("sync.sh").resolve()

BUN_STUB = r'''#!/usr/bin/env python3
from pathlib import Path
import subprocess
import sys

scratch = Path(sys.argv[2])
root = Path(sys.argv[3]).resolve()
name = root.name
files = subprocess.check_output(
    ["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
(scratch / f"{name}.files").write_bytes(files + b"\0")
(scratch / "repos").write_text(f"{name}\t{root}\n")
sys.stdin.read()
'''

SSH_STUB = r'''#!/usr/bin/env python3
import fcntl
import os
from pathlib import Path
import shlex
import subprocess
import sys

remote = Path(os.environ["REMOTE_ROOT"])
remote.mkdir(parents=True, exist_ok=True)
command = sys.argv[-1]
script = sys.stdin.read()
lock = remote / "runner.lock"

def event(name):
    with (remote / "events.log").open("a") as stream:
        stream.write(name + "\n")

if "/srv/build/runner.lock.guard" in command:
    args = shlex.split(command)
    separator = args.index("--")
    translated_script = script.replace("/srv/build", str(remote))
    guard_path = remote / "runner.lock.guard"
    with guard_path.open("a") as guard:
        fcntl.flock(guard, fcntl.LOCK_EX)
        result = subprocess.run(
            ["bash", "-s", "--", *args[separator + 1:]],
            input=translated_script, text=True, capture_output=True)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    sys.exit(result.returncode)

if command == "mkdir /srv/build/runner.lock":
    try:
        lock.mkdir()
    except FileExistsError:
        print("Runner busy (or not set up)", file=sys.stderr)
        sys.exit(1)
    event("lock-acquired-legacy")
    sys.exit(0)

if command == "rmdir /srv/build/runner.lock":
    try:
        lock.rmdir()
    except FileNotFoundError:
        pass
    event("lock-released-legacy")
    sys.exit(0)


if "git --git-dir=/srv/build/mirrors/" in command:
    sys.stdout.write("present\n")
    sys.exit(0)

if command.startswith("bash -s -- "):
    event("remote-update")
    sys.exit(0)

print(f"Unhandled stand-in SSH command: {command}", file=sys.stderr)
sys.exit(2)
'''

RSYNC_STUB = r'''#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import sys
import time

args = sys.argv[1:]
remote = Path(os.environ["REMOTE_ROOT"])
source, destination = args[-2:]


def copy_file(src, dst):
    dst.parent.mkdir(parents=True, exist_ok=True)
    if src.is_symlink():
        if dst.exists() or dst.is_symlink():
            dst.unlink()
        dst.symlink_to(os.readlink(src))
    else:
        shutil.copy2(src, dst)

if destination.startswith(os.environ["RUNNER_HOST"] + ":"):
    dest_path = remote / destination.split(":", 1)[1].lstrip("/")
    event = remote / "transfer-started"
    event.write_text(str(os.getpid()))
    if os.environ.get("HOLD_TRANSFER") == "1":
        parent = os.getppid()
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if os.environ.get("ALLOW_TRANSFER") and Path(os.environ["ALLOW_TRANSFER"]).exists():
                break
            if os.getppid() != parent:
                (remote / "transfer-abandoned").write_text("parent exited")
                sys.exit(0)
            time.sleep(0.01)
    if "--delete" in args and dest_path.exists():
        source_root = Path(source.rstrip("/"))
        incoming = {str(path.relative_to(source_root)) for path in source_root.rglob("*")}
        for existing in sorted(dest_path.rglob("*"), reverse=True):
            relative = str(existing.relative_to(dest_path))
            if relative not in incoming:
                if existing.is_dir() and not existing.is_symlink():
                    try:
                        existing.rmdir()
                    except OSError:
                        pass
                else:
                    existing.unlink()
    dest_path.mkdir(parents=True, exist_ok=True)
    for item in Path(source.rstrip("/")).rglob("*"):
        relative = item.relative_to(Path(source.rstrip("/")))
        if relative.parts and relative.parts[0] == ".git":
            continue
        if item.is_file() or item.is_symlink():
            copy_file(item, dest_path / relative)
        elif item.is_dir():
            (dest_path / relative).mkdir(parents=True, exist_ok=True)
    with (remote / "rsync-remote-args.txt").open("a") as stream:
        stream.write(" ".join(args) + "\n")
    sys.exit(0)

files_option = next((item.split("=", 1)[1] for item in args if item.startswith("--files-from=")), None)
if files_option is None:
    print("Missing --files-from in local staging transfer", file=sys.stderr)
    sys.exit(2)
file_list = Path(files_option).read_bytes().split(b"\0")
source_root = Path(source.rstrip("/"))
destination_root = Path(destination.rstrip("/"))
for encoded in file_list:
    if not encoded:
        continue
    relative = Path(os.fsdecode(encoded))
    copy_file(source_root / relative, destination_root / relative)
'''


class SyncLockTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = self.root / "sample"
        self.repo.mkdir()
        (self.repo / ".gitignore").write_text("ignored.txt\n")
        (self.repo / "tracked.txt").write_text("tracked\n")
        (self.repo / "ignored.txt").write_text("tracked under ignored directory\n")
        self.run_git("init", "-q")
        self.run_git("config", "user.email", "sync-lock@example.invalid")
        self.run_git("config", "user.name", "Sync Lock Test")
        self.run_git("add", ".gitignore", "tracked.txt")
        self.run_git("add", "-f", "ignored.txt")
        self.run_git("commit", "-qm", "fixture")

        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.write_executable("bun", BUN_STUB)
        self.write_executable("ssh", SSH_STUB)
        self.write_executable("rsync", RSYNC_STUB)
        self.remote = self.root / "server"
        stale = self.remote / "srv/build/src/sample"
        stale.mkdir(parents=True)
        (stale / "stale.txt").write_text("remove me\n")
        self.allow_transfer = self.root / "allow-transfer"
        self.processes = []
        self.base_env = dict(os.environ)
        for key in ("GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"):
            self.base_env.pop(key, None)
        self.base_env.update({
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "RUNNER_HOST": "test-host",
            "RUNNER_LOCK_STALE_AFTER_S": "5",
            "REMOTE_ROOT": str(self.remote),
            "GIT_CONFIG_NOSYSTEM": "1",
        })

    def tearDown(self):
        for process in self.processes:
            if process.poll() is None:
                os.kill(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            process.communicate(timeout=5)
        self.temp.cleanup()

    def run_git(self, *args):
        subprocess.run(["git", "-C", str(self.repo), *args], check=True,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def write_executable(self, name, content):
        path = self.bin / name
        path.write_text(textwrap.dedent(content))
        path.chmod(0o755)

    def start_sync(self, *, hold_transfer=False, stale_after="5"):
        env = dict(self.base_env, RUNNER_LOCK_STALE_AFTER_S=stale_after)
        if hold_transfer:
            env["HOLD_TRANSFER"] = "1"
            env["ALLOW_TRANSFER"] = str(self.allow_transfer)
        process = subprocess.Popen(["bash", str(SYNC), str(self.repo)], env=env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, start_new_session=True)
        self.processes.append(process)
        return process

    def wait_for(self, path, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if path.exists():
                return
            time.sleep(0.01)
        self.fail(f"timed out waiting for {path.name}")

    def test_killed_sync_recovers_stale_lock_and_next_sync_proceeds(self):
        first = self.start_sync(hold_transfer=True, stale_after="1")
        self.wait_for(self.remote / "transfer-started")
        self.assertTrue((self.remote / "runner.lock").exists())

        os.kill(first.pid, signal.SIGKILL)
        first.wait(timeout=5)
        self.wait_for(self.remote / "transfer-abandoned")
        first.communicate(timeout=5)
        time.sleep(1.2)

        second = self.start_sync(stale_after="1")
        stdout, stderr = second.communicate(timeout=20)
        self.assertEqual(second.returncode, 0, f"stdout={stdout}\nstderr={stderr}")
        self.assertIn("Reclaiming stale source-copy lock", stderr)
        self.assertIn("test-host", stderr)
        self.assertTrue((self.remote / "srv/build/src/sample/ignored.txt").exists())
        self.assertFalse((self.remote / "srv/build/src/sample/stale.txt").exists())
        transfer_args = (self.remote / "rsync-remote-args.txt").read_text()
        self.assertIn("--delete", transfer_args)
        self.assertIn("--exclude=/.git/", transfer_args)

    def test_live_syncs_do_not_overlap_and_staging_keeps_delete_semantics(self):
        first = self.start_sync(hold_transfer=True, stale_after="5")
        self.wait_for(self.remote / "transfer-started")

        second = self.start_sync(stale_after="5")
        stdout, stderr = second.communicate(timeout=10)
        self.assertNotEqual(second.returncode, 0, f"unexpected success: {stdout}")
        self.assertIn("busy", stderr.lower())

        self.allow_transfer.touch()
        stdout, stderr = first.communicate(timeout=20)
        self.assertEqual(first.returncode, 0, f"stdout={stdout}\nstderr={stderr}")
        self.assertFalse((self.remote / "srv/build/src/sample/stale.txt").exists())
        self.assertTrue((self.remote / "srv/build/src/sample/ignored.txt").exists())
        transfer_args = (self.remote / "rsync-remote-args.txt").read_text()
        self.assertIn("--delete", transfer_args)
        self.assertIn("--exclude=/.git/", transfer_args)


if __name__ == "__main__":
    unittest.main()
