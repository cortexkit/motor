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


class RunnerBridgeUnavailable(RuntimeError):
    pass


class RunnerBridgeRefused(RuntimeError):
    pass


def runner_service_status():
    command = [r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
               "$ErrorActionPreference='Stop'; (Get-Service -Name MotorRunner -ErrorAction Stop).Status.ToString()"]
    try:
        result = subprocess.run(command, capture_output=True, encoding="utf-8", errors="replace", timeout=10)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(f"cannot query runner service MotorRunner: {error}") from error
    state = result.stdout.strip()
    if result.returncode or state not in {"Stopped", "StartPending", "StopPending", "Running", "ContinuePending", "PausePending", "Paused"}:
        raise RuntimeError(f"cannot query runner service MotorRunner: exit={result.returncode} stdout={result.stdout!r} stderr={result.stderr!r}")
    return state


def require_runner_service():
    state = runner_service_status()
    if state != "Running":
        raise RunnerBridgeUnavailable(f"runner service not running (MotorRunner status={state}); check service startup and parent verification")
    return state


class Channel:
    def __init__(self, runner, root, log, hello_timeout: float = 30):
        require_runner_service()
        self.log_path = log
        self.log = log.open("wb")
        try:
            self.process = subprocess.Popen([str(runner), "connect", "--instance", "ally", "--state-root", str(root / "state")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
        except Exception:
            self.log.close()
            raise
        self.lock = threading.Lock()
        self.messages = queue.Queue()
        self.stopped = threading.Event()
        try:
            threading.Thread(target=self.read, daemon=True).start()
            self.send({"type": "hello", "protocol_version": 1, "started": True, "gate_jobs": True})
            hello = self.until("hello", timeout=hello_timeout)
            if hello.get("protocol_version") != 1 or hello.get("platforms") != ["windows"]:
                raise RuntimeError(f"wrong runner Hello version/platform: {hello}")
        except Exception as error:
            classified = error
            if isinstance(error, (OSError, TimeoutError)):
                try:
                    classified = RunnerBridgeUnavailable(self.bridge_failure(str(error)))
                except Exception as diagnostic_error:
                    classified = diagnostic_error
            try:
                self.close()
            except Exception as cleanup_error:
                raise RuntimeError(f"{classified}; bridge cleanup failed: {cleanup_error}") from error
            if classified is not error:
                raise classified from error
            raise
        threading.Thread(target=self.heartbeat, daemon=True).start()

    def bridge_failure(self, reason):
        try:
            state = runner_service_status()
            service = f"runner service not running (MotorRunner status={state})" if state != "Running" else "runner service is Running but its bridge is not ready; startup/parent verification may still be in progress"
        except Exception as error:
            service = f"runner service status unavailable: {error}"
        self.log.flush()
        detail = self.log_path.read_text(encoding="utf-8", errors="replace").strip()
        if "local pipe is not owned by the running MotorRunner service" in detail:
            raise RunnerBridgeRefused(f"runner bridge refused a non-service pipe; log={self.log_path}; diagnostics={detail}")
        return f"{service}; {reason}; bridge exit={self.process.poll()}; log={self.log_path}; diagnostics={detail or '<empty>'}"

    def send(self, value):
        data = json.dumps(value, separators=(",", ":")).encode()
        pipe = self.process.stdin
        if pipe is None:
            raise RuntimeError("runner bridge has no stdin pipe")
        with self.lock:
            pipe.write(struct.pack(">I", len(data)) + data)
            pipe.flush()

    def read(self):
        try:
            def exact(n):
                pipe = self.process.stdout
                if pipe is None:
                    raise RuntimeError("runner bridge has no stdout pipe")
                chunks = bytearray()
                while len(chunks) < n:
                    data = pipe.read(n - len(chunks))
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

    def until(self, kind, output=None, timeout: float = 900):
        deadline = time.monotonic() + timeout
        while True:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"runner did not deliver {kind} within {timeout} seconds")
            try:
                value = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty as error:
                raise TimeoutError(f"runner did not deliver {kind} within {timeout} seconds") from error
            if isinstance(value, EOFError):
                raise RunnerBridgeUnavailable(self.bridge_failure(str(value))) from value
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
        try:
            if self.process.poll() is None:
                self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        finally:
            for pipe in (self.process.stdin, self.process.stdout):
                if pipe is not None:
                    pipe.close()
            self.log.close()


def wait_for_service_ready(root, runner, evidence, timeout=180):
    started = time.monotonic()
    observations = []
    ready = False
    failure = None
    def save():
        (evidence / "service-readiness.json").write_text(json.dumps(dict(ready=ready, failure=failure, elapsed_seconds=time.monotonic() - started, observations=observations), indent=2), encoding="utf-8")
    try:
        save()
        while time.monotonic() - started < timeout:
            state = runner_service_status()
            observation = dict(host_unix_ms=int(time.time() * 1000), status=state)
            observations.append(observation)
            save()
            if state not in {"Running", "StartPending"}:
                raise RunnerBridgeUnavailable(f"runner service not running (MotorRunner status={state}); startup/parent verification did not complete")
            if state == "Running":
                try:
                    channel = Channel(runner, root, evidence / f"bridge-ready-{len(observations)}.log", hello_timeout=min(5, max(0.01, timeout - (time.monotonic() - started))))
                except RunnerBridgeUnavailable as error:
                    observation["bridge_error"] = str(error)
                else:
                    channel.close()
                    ready = True
                    observation["hello_validated"] = True
                    # The runner binds this pipe only after Runner::open has
                    # finished parent validation and reconciliation. A Running
                    # service status alone is earlier, not a readiness signal.
                    print("PASS runner-service-ready parent-verification-complete windows-protocol-hello", flush=True)
                    return
            save()
            time.sleep(min(0.5, max(0, timeout - (time.monotonic() - started))))
        raise TimeoutError(f"runner service did not provide a valid Windows Hello within {timeout} seconds; inspect service startup/parent verification and readiness bridge logs")
    except Exception as error:
        failure = str(error)
        raise
    finally:
        try:
            save()
        except Exception:
            if failure is None:
                raise


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


def fixture_bundle(repo, bundle, commit):
    # Match Snapshots::bundle's temporary transport refs. The runner rejects
    # ordinary branch names and HEAD; this fixture's snapshot and tip are the
    # same commit, so one snapshot ref supplies both required request fields.
    bundle_ref = f"refs/ck-motor/{uuid.uuid4()}/snapshot"
    git(repo, "update-ref", bundle_ref, commit)
    try:
        git(repo, "bundle", "create", str(bundle), bundle_ref)
    finally:
        git(repo, "update-ref", "-d", bundle_ref)


def run(root, runner, evidence, gate):
    require_runner_service()
    repo = evidence / "fixture"
    commit, tree = fixture(repo)
    bundle = evidence / "fixture.bundle"
    fixture_bundle(repo, bundle, commit)
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


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(r"C:\ProgramData\MotorRunner"))
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--gate-command", default="cargo test --locked --offline -- --nocapture && cargo clippy --all-targets --locked --offline -- -D warnings")
    parser.add_argument("--wait-ready", action="store_true", help="Wait for service Running and a validated Windows Hello after parent verification")
    args = parser.parse_args(argv)
    args.evidence.mkdir(parents=True, exist_ok=False)
    if args.wait_ready:
        wait_for_service_ready(args.root, args.root / "bin/motor-runner.exe", args.evidence)
    else:
        run(args.root, args.root / "bin/motor-runner.exe", args.evidence, args.gate_command)


if __name__ == "__main__":
    main()
