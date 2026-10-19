"""Mount fences and measured, per-version storage admission for image builds."""
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import threading
import time
from typing import Any

GIB = 1024**3
FREE_FLOOR = 170 * GIB


def read_mountinfo():
    return Path("/proc/self/mountinfo").read_text()


def mounts_under(root, text):
    root = Path(os.path.abspath(root))
    result = []
    lines = text.splitlines()
    if not lines:
        raise ValueError("empty mountinfo cannot establish that a stage is unmounted")
    for number, line in enumerate(lines, 1):
        fields = line.split()
        if len(fields) < 10 or " - " not in line or not fields[0].isdigit():
            raise ValueError(f"malformed mountinfo at line {number}")
        path = Path(re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), fields[4]))
        if not path.is_absolute():
            raise ValueError(f"nonabsolute mountpoint in mountinfo at line {number}")
        if path == root or root in path.parents:
            result.append((int(fields[0]), path))
    return sorted(result, key=lambda row: (len(row[1].parts), row[0]), reverse=True)


def assert_unmounted(root, reader=read_mountinfo):
    remaining = mounts_under(root, reader())
    if remaining:
        raise RuntimeError("refusing stage removal with mounted paths: " + ", ".join(str(path) for _, path in remaining))


def unmount_tree(root, reader=read_mountinfo, execute=subprocess.run):
    remaining = mounts_under(root, reader())
    attempts = 0
    while remaining:
        path = remaining[0][1]
        try:
            execute(["umount", "--", str(path)], check=True, capture_output=True, text=True)
        except subprocess.CalledProcessError as error:
            raise RuntimeError(f"blocking umount failed for {path}: exit {error.returncode}; {error.stderr}") from error
        updated = mounts_under(root, reader())
        if updated == remaining:
            raise RuntimeError(f"mount remains after blocking umount: {path}")
        remaining = updated
        attempts += 1
        if attempts >= 4096 and remaining:
            raise RuntimeError(f"mount tree kept changing during cleanup: {remaining[0][1]}")
    assert_unmounted(root, reader)
    print(f"chroot cleanup: {attempts} blocking unmounts; no mounts below {root}", flush=True)


def allocated_bytes(root, reader=read_mountinfo):
    root = Path(os.path.abspath(root))
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError(f"version root is not a directory: {root}")
    blocked = {path for _, path in mounts_under(root, reader())}
    if root in blocked:
        raise RuntimeError(f"cannot account through a mount at the version root: {root}")
    seen = set()
    total = 0
    pending = [root]
    while pending:
        path = pending.pop()
        if path in blocked:
            continue
        try:
            info = path.lstat()
            identity = (info.st_dev, info.st_ino)
            if identity in seen:
                continue
            seen.add(identity)
            total += info.st_blocks * 512
            if stat.S_ISDIR(info.st_mode):
                with os.scandir(path) as entries:
                    pending.extend(Path(entry.path) for entry in entries)
        except FileNotFoundError:
            if path == root:
                raise
            # Compiler temporaries can disappear during a sample. Never follow
            # a symlink or descend through a mount to compensate for that race.
            continue
    return total


def choose_budget(explicit_bytes, profile, recipe):
    if (explicit_bytes is None) == (profile is None):
        raise ValueError("choose exactly one explicit build budget or measured peak profile")
    if explicit_bytes is not None:
        if type(explicit_bytes) is not int or explicit_bytes <= 0:
            raise ValueError("build budget must be positive bytes")
        return explicit_bytes, "operator bootstrap cap (not a measured requirement)"
    data = json.loads(Path(profile).read_text())
    peak = data.get("peak_allocated_bytes")
    if (data.get("schema") != 1 or data.get("outcome") != "success" or data.get("measurement_errors") != []
            or data.get("recipe_digest") != recipe or type(peak) is not int or peak <= 0
            or type(data.get("sample_count")) is not int or data["sample_count"] <= 0):
        raise ValueError("peak profile must be successful, complete and match this image recipe, runner and job count")
    padded = (peak * 5 + 3) // 4
    return ((padded + GIB - 1) // GIB + 4) * GIB, f"measured peak +25% rounded up +4 GiB: {profile}"


class DiskBudgetError(RuntimeError):
    pass


class DiskMeter:
    def __init__(self, target, budget, recipe, floor=FREE_FLOOR, interval: float = 1, reader=read_mountinfo, usage=None):
        import shutil
        self.target = target
        self.budget, self.recipe, self.floor, self.interval = budget, recipe, floor, interval
        self.reader, self.usage = reader, usage or shutil.disk_usage
        self.peak = 0
        self.minimum_free = None
        self.samples = 0
        self.errors = []
        self.violation = None
        self.stop_event = threading.Event()
        self.lock = threading.Lock()
        self.thread = None
        self.started_at = time.monotonic()

    def persist(self, outcome) -> dict[str, Any]:
        data = dict(schema=1, recipe_digest=self.recipe, outcome=outcome,
                    budget_bytes=self.budget, free_floor_bytes=self.floor,
                    peak_allocated_bytes=self.peak, minimum_free_bytes=self.minimum_free,
                    sample_count=self.samples, sampling_interval_s=self.interval,
                    elapsed_s=time.monotonic() - self.started_at, measurement_errors=self.errors,
                    accounting="sampled allocated file blocks; hardlinks deduplicated; mounts/symlinks not followed; reflink sharing may overcount")
        path = self.target / "disk-usage.json"
        temporary = path.with_suffix(".pending")
        temporary.write_text(json.dumps(data, indent=2) + "\n")
        if outcome == "success":
            temporary.chmod(0o444)
            with temporary.open("rb") as source:
                os.fsync(source.fileno())
        os.replace(temporary, path)
        if outcome == "success":
            directory = os.open(self.target, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        return data

    def sample(self):
        with self.lock:
            try:
                owned = allocated_bytes(self.target, self.reader)
                free = self.usage(self.target).free
                self.peak = max(self.peak, owned)
                self.minimum_free = free if self.minimum_free is None else min(self.minimum_free, free)
                self.samples += 1
                if owned > self.budget:
                    self.violation = f"allocated build bytes {owned} exceed explicit/admitted cap {self.budget}"
                if free < self.floor:
                    self.violation = f"filesystem free bytes {free} fell below the {self.floor}-byte floor"
                self.persist("building")
            except Exception as error:
                self.errors.append(f"{type(error).__name__}: {error}")
                self.violation = "cannot measure build storage: " + self.errors[-1]

    def check(self):
        if self.violation:
            raise DiskBudgetError(self.violation)

    def checkpoint(self, phase):
        self.sample()
        print(f"build storage checkpoint {phase}: sampled peak={self.peak} allocated bytes, minimum free={self.minimum_free}", flush=True)
        self.check()

    def start(self):
        self.checkpoint("start")
        def poll():
            while not self.stop_event.wait(self.interval):
                self.sample()
        self.thread = threading.Thread(target=poll, name="image-disk-meter", daemon=True)
        self.thread.start()

    def stop(self):
        self.stop_event.set()
        if self.thread is not None:
            self.thread.join()

    def run(self, argv, check=True, env=None):
        self.check()
        proc = subprocess.Popen(argv, env=env, start_new_session=True)
        try:
            while proc.poll() is None:
                self.stop_event.wait(.1)
                self.check()
            self.checkpoint(Path(str(argv[0])).name + " command complete")
            if check and proc.returncode:
                raise subprocess.CalledProcessError(proc.returncode, argv)
            return subprocess.CompletedProcess(argv, proc.returncode)
        except BaseException:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(proc.pid, sig)
                except ProcessLookupError:
                    pass
                if sig == signal.SIGTERM:
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        pass
            proc.wait(timeout=10)
            raise

    def finish(self, outcome):
        self.stop()
        self.sample()
        if outcome == "success" and self.violation:
            outcome = "failed"
        data = self.persist(outcome)
        print("build disk usage: " + json.dumps(data), flush=True)
        if outcome == "failed" and self.violation:
            self.check()
        return data
