#!/usr/bin/env python3
"""Operator-only real runner protocol qualification; no Cargo or rustup on the host."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import struct
import subprocess
import tarfile
import threading
import time
import uuid


def git(root, *args):
    return subprocess.check_output([r"C:\Program Files\Git\cmd\git.exe", "-C", str(root), *args], stderr=subprocess.STDOUT).decode().strip()


class Channel:
    def __init__(self, runner, root, log):
        self.log = log.open("wb")
        self.process = subprocess.Popen([str(runner), "connect", "--instance", "ally", "--state-root", str(root / "state")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
        self.lock = threading.Lock()
        self.messages = queue.Queue()
        self.stopped = threading.Event()
        threading.Thread(target=self.read, daemon=True).start()
        self.send({"type": "hello", "protocol_version": 1, "started": True, "gate_jobs": True})
        hello = self.until("hello")
        if hello.get("platforms") != ["windows"]:
            raise RuntimeError(f"wrong runner platform: {hello}")
        threading.Thread(target=self.heartbeat, daemon=True).start()

    def send(self, value):
        data = json.dumps(value, separators=(",", ":")).encode()
        with self.lock:
            self.process.stdin.write(struct.pack(">I", len(data)) + data)
            self.process.stdin.flush()

    def read(self):
        try:
            def exact(n):
                chunks = bytearray()
                while len(chunks) < n:
                    data = self.process.stdout.read(n - len(chunks))
                    if not data:
                        raise EOFError("runner bridge closed")
                    chunks.extend(data)
                return chunks
            while True:
                length = struct.unpack(">I", exact(4))[0]
                if not 0 < length <= 1024 * 1024:
                    raise ValueError("runner frame length outside protocol cap")
                self.messages.put(json.loads(exact(length)))
        except Exception as error:
            self.messages.put(error)

    def heartbeat(self):
        while not self.stopped.wait(1):
            try:
                self.send({"type": "heartbeat"})
            except (OSError, ValueError):
                return

    def until(self, kind, output=None):
        deadline = time.monotonic() + 900
        while True:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"runner did not deliver {kind} within 900 seconds")
            value = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            if isinstance(value, Exception):
                raise value
            if value["type"] == "refusal":
                raise RuntimeError(f"runner refused qualification: {value}")
            if value["type"] == "output" and output is not None:
                output.extend(base64.b64decode(value["bytes"], validate=True))
            if value["type"] == kind:
                return value

    def close(self):
        self.stopped.set()
        self.process.terminate()
        self.process.wait(timeout=20)
        self.log.close()


def fixture(path):
    path.mkdir()
    (path / "src").mkdir()
    (path / "Cargo.toml").write_text('[package]\nname="hyperv-qualification"\nversion="0.1.0"\nedition="2021"\n')
    (path / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "hyperv-qualification"\nversion = "0.1.0"\n')
    (path / "src/lib.rs").write_text(r'''
#[test]
fn real_gate_standard_account_cache_boundary_and_large_collection() {
    use std::io::Write;
    assert_eq!(std::env::var("USERNAME").unwrap(), "motor-job");
    assert!(!std::path::Path::new(r"C:\ProgramData\MotorRunner").exists());
    let home = std::env::var("CARGO_HOME").unwrap();
    assert!(!std::path::Path::new(&home).join(".host-only-canary").exists());
    std::fs::write("large-output.bin", vec![b'c'; 2 * 1024 * 1024]).unwrap();
    std::io::stdout().write_all(&vec![b'Z'; 1024 * 1024]).unwrap();
}
''')
    (path / "multi-frame.bin").write_bytes(os.urandom(2 * 1024 * 1024))
    (path / "large-output.bin").write_bytes(b'x' * (2 * 1024 * 1024))
    git(path, "init", "-b", "main")
    git(path, "-c", "user.name=Qualification", "-c", "user.email=qualification@invalid", "add", ".")
    git(path, "-c", "user.name=Qualification", "-c", "user.email=qualification@invalid", "commit", "-m", "qualification fixture")
    return git(path, "rev-parse", "HEAD"), git(path, "rev-parse", "HEAD^{tree}")


def run(root, runner, evidence, gate):
    repo = evidence / "fixture"
    commit, tree = fixture(repo)
    bundle = evidence / "fixture.bundle"
    git(repo, "bundle", "create", str(bundle), "--all")
    data = bundle.read_bytes()
    if len(data) <= 65536:
        raise RuntimeError("fixture did not produce a multi-frame bundle")
    canary = root / "state/ally/prep-cargo/.host-only-canary"
    canary.parent.mkdir(parents=True, exist_ok=True)
    canary.write_text("host-private-prep-home")
    samples = []
    for iteration in range(5):
        job = str(uuid.uuid4())
        channel = Channel(runner, root, evidence / f"bridge-{iteration}.log")
        try:
            command = ("test -f /c/t/qualification-cache-marker && " if iteration else "") + gate + " && printf cached > /c/t/qualification-cache-marker"
            request = dict(type="run", job_id=job, gate_job=True, platform="windows", workspace_key="/qualification/workspace", repository_root="/qualification/workspace", cwd="/qualification/workspace", command=command, env={}, weight_hint=4, timeout=600, queue_wait_limit_s=60, snapshot_commit=commit, tree_hash=tree, integration_tip=commit, siblings=[], git_dependencies=[], bundles=[dict(index=0, repository="/qualification/workspace", length=len(data), sha256=hashlib.sha256(data).hexdigest())])
            channel.send(request)
            channel.until("accepted")
            for offset in range(0, len(data), 65536):
                channel.send(dict(type="bundle_chunk", index=0, offset=offset, data=base64.b64encode(data[offset:offset+65536]).decode()))
            output = bytearray()
            terminal = channel.until("terminal", output)
            (evidence / f"terminal-{iteration}.json").write_text(json.dumps(terminal, indent=2))
            (evidence / f"output-{iteration}.bin").write_bytes(output)
            if terminal.get("outcome") != {"type": "exit", "code": 0}:
                raise RuntimeError(f"gate failed: {terminal}")
            if output.count(b'Z') < 1024 * 1024:
                raise RuntimeError("large stdout collection was truncated")
            collection = root / "state/ally/jobs" / job / "windows-changes.tar"
            with tarfile.open(collection) as archive:
                member = archive.extractfile("m/qualification/workspace/large-output.bin")
                if member is None or member.read() != b'c' * (2 * 1024 * 1024):
                    raise RuntimeError("large source output collection is missing or corrupt")
            event_file = root / "evidence" / f"{job}.jsonl"
            events = [json.loads(line.lstrip('\ufeff')) for line in event_file.read_text(encoding="utf-8-sig").splitlines()]
            for expected in ("created-offline", "host-isolation-sample", "normal-off", "nothing-left-behind"):
                if not any(event["event"] == expected for event in events):
                    raise RuntimeError(f"missing native host evidence: {expected}")
            created = next(event for event in events if event["event"] == "created-offline")
            cache = Path(created["detail"]["cache"])
            if not cache.is_file() or cache.with_suffix(".attached").exists():
                raise RuntimeError("cache missing or still marked attached after normal completion")
            samples.append(dict(job_id=job, bundle_bytes=len(data), stdout_bytes=len(output), cache=str(cache), terminal=terminal))
            print(f"PASS real-gate iteration={iteration} job={job} bundle_bytes={len(data)} output_bytes={len(output)} cache={cache}", flush=True)
        finally:
            channel.close()
    if len({sample["cache"] for sample in samples}) != 1:
        raise RuntimeError("warm job did not reuse the real workspace cache disk")
    (evidence / "real-jobs.json").write_text(json.dumps(samples, indent=2))
    print("PASS multi-frame-tree real-cache-reuse large-stdout large-source-collection standard-account prep-home-not-exposed real-gate-shape", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(r"C:\ProgramData\MotorRunner"))
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--gate-command", default="cargo test --locked --offline -- --nocapture && cargo clippy --all-targets --locked --offline -- -D warnings")
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=False)
    run(args.root, args.root / "bin/motor-runner.exe", args.evidence, args.gate_command)
