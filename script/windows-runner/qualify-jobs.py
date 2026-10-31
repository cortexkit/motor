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


def require_windows_guest_agent_ready(status):
    health = status.get("health") if isinstance(status, dict) else None
    agent = health.get("windows_guest_agent") if isinstance(health, dict) else None
    if not isinstance(agent, dict) or agent.get("ready") is not True:
        detail = agent.get("detail") if isinstance(agent, dict) else None
        raise RunnerBridgeRefused(f"Windows guest agent not ready: {detail or 'identity status missing'}")
    return agent


class Channel:
    def __init__(self, runner, root, log, hello_timeout: float = 30, check_agent_readiness: bool = False):
        require_runner_service()
        self.log_path = log
        self.log = log.open("wb")
        try:
            self.process = subprocess.Popen([str(runner), "connect", "--instance", "ally-qualification", "--state-root", str(root / "state")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
        except Exception:
            self.log.close()
            raise
        self.lock = threading.Lock()
        self.messages = queue.Queue()
        self.stopped = threading.Event()
        try:
            threading.Thread(target=self.read, daemon=True).start()
            self.send({"type": "hello", "protocol_version": 1, "started": True, "gate_jobs": True, "return_paths": True, "return_chunked": True})
            hello = self.until("hello", timeout=hello_timeout)
            if hello.get("protocol_version") != 1 or hello.get("platforms") != ["windows"]:
                raise RuntimeError(f"wrong runner Hello version/platform: {hello}")
            if check_agent_readiness:
                self.guest_agent_readiness = require_windows_guest_agent_ready(
                    self.request_status(hello_timeout)
                )
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

    def request_status(self, timeout):
        self.send({"type": "status"})
        return self.until("status_reply", timeout=timeout)

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

    def until(self, kind, output=None, timeout: float = 900, frames=None):
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
            if frames is not None:
                frames.append(value)
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
                    channel = Channel(runner, root, evidence / f"bridge-ready-{len(observations)}.log", hello_timeout=min(5, max(0.01, timeout - (time.monotonic() - started))), check_agent_readiness=True)
                except RunnerBridgeRefused as error:
                    observation["guest_agent_not_ready"] = str(error)
                    raise
                except RunnerBridgeUnavailable as error:
                    observation["bridge_error"] = str(error)
                else:
                    channel.close()
                    ready = True
                    observation["hello_validated"] = True
                    # Runner readiness includes the sealed guest-agent identity check.
                    print("PASS runner-service-ready parent-verification-complete windows-protocol-hello guest-agent-ready", flush=True)
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


def require_gate_success(terminal):
    # Windows jobs report the root process's full 32-bit status as
    # windows_exit, never the Unix exit variant.
    if terminal.get("outcome") != {"type": "windows_exit", "code": 0}:
        raise RuntimeError(f"gate failed: {terminal}")


BUN_VERSION_CHECK = "bun_version=$(bun --version) && test \"${bun_version%$'\\r'}\" = \"1.4.4\""
JQ_VERSION_CHECK = "jq_version=$(jq --version) && printf '%s\\n' \"$jq_version\" && test \"${jq_version%$'\\r'}\" = 'jq-1.8.2'"
# Run the alias and native lookup probes inside the standard user's logon
# session; an elevated provisioning shell cannot prove that jobs see the drive.
CACHE_DRIVE_SCRIPT = r"""
$ErrorActionPreference='Stop'
if((Get-Command jq.exe -ErrorAction Stop).Source -cne 'C:\Program Files\jq\jq.exe'){throw 'jq resolves outside the protected installation'}
$version=& jq.exe --version
if($LASTEXITCODE -ne 0 -or $version -cne 'jq-1.8.2'){throw 'native jq version mismatch'}
if(-not (Test-Path -LiteralPath 'T:\' -PathType Container)){throw 'cache drive T: is missing'}
if(((Get-Item -LiteralPath 'T:\' -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'cache drive root is a reparse point'}
$query=(& fsutil.exe reparsepoint query 'T:\' 2>&1 | Out-String)
if($LASTEXITCODE -eq 0 -or $query -notmatch 'not a reparse point'){throw "cache drive reparse query failed: $query"}
if((Get-Acl -LiteralPath 'T:\').Sddl -cne (Get-Acl -LiteralPath 'C:\t\').Sddl){throw 'cache drive and folder permissions differ'}
$probe='motor-drive-probe-'+[Guid]::NewGuid().ToString()
try {
 [IO.File]::WriteAllText(('T:\'+$probe),'drive cache')
 if([IO.File]::ReadAllText(('C:\t\'+$probe)) -cne 'drive cache'){throw 'cache drive write did not reach folder mount'}
 [IO.File]::WriteAllText(('C:\t\'+$probe),'folder cache')
 if([IO.File]::ReadAllText(('T:\'+$probe)) -cne 'folder cache'){throw 'cache folder write did not reach drive mount'}
} finally {Remove-Item -LiteralPath ('T:\'+$probe) -Force -ErrorAction SilentlyContinue}
Write-Output 'cache drive aliases share permissions and data; native jq verified'
"""
CACHE_DRIVE_CHECK = "pwsh -NoProfile -NonInteractive -EncodedCommand " + base64.b64encode(CACHE_DRIVE_SCRIPT.encode("utf-16le")).decode("ascii")
NEXTEST_VERSION_CHECK = "nextest_version=$(cargo nextest --version) && printf '%s\\n' \"$nextest_version\" && [[ \"$nextest_version\" == 'cargo-nextest 0.9.138 '* ]]"
PYTHON_CHECK = (
    "python_version=$(python3 --version) && printf '%s\\n' \"$python_version\" && test \"${python_version%$'\\r'}\" = 'Python 3.14.8' && "
    "python_version=$(python --version) && test \"${python_version%$'\\r'}\" = 'Python 3.14.8' && "
    'python3 -c "import json, hashlib, subprocess, tomllib, compression.zstd"'
)
NATS_VERSION_CHECK = "nats_version=$(nats-server --version) && printf '%s\\n' \"$nats_version\" && test \"${nats_version%$'\\r'}\" = 'nats-server: v2.15.0'"
LLVM_CHECK = (
    "clang_version=$(clang --version) && printf '%s\\n' \"$clang_version\" && "
    "clang_first=${clang_version%%$'\\n'*} && test \"${clang_first%$'\\r'}\" = 'clang version 21.1.8' && "
    "test \"$LIBCLANG_PATH\" = 'C:\\Program Files\\LLVM\\bin' && "
    "test -f '/c/Program Files/LLVM/bin/libclang.dll'"
)
STABLE_VERSION_CHECK = "stable_version=$(rustc +stable --version) && printf '%s\\n' \"$stable_version\" && [[ \"$stable_version\" == 'rustc 1.99.0 '* ]]"


GIT_DEP_URL = "https://dep.invalid/itoa-fixture"


def commit_fixture(path):
    git(path, "init", "-b", "main")
    git(path, "add", ".")
    git(path, "-c", "user.name=Qualification", "-c", "user.email=qualification@invalid", "commit", "-m", "qualification fixture")
    return git(path, "rev-parse", "HEAD"), git(path, "rev-parse", "HEAD^{tree}")


def dependency_fixture(path, dependency):
    dependency.mkdir()
    (dependency / "src").mkdir()
    (dependency / "Cargo.toml").write_text('[package]\nname="itoa-fixture"\nversion="1.0.11"\nedition="2021"\n')
    (dependency / "src/lib.rs").write_text('pub fn value() -> u32 { 11 }\n')
    dep_commit, _ = commit_fixture(dependency)
    git(dependency, "tag", "1.0.11")
    path.mkdir()
    (path / "src").mkdir()
    (path / "Cargo.toml").write_text(f'[package]\nname="git-tag-qualification"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nitoa-fixture = {{ git = "{GIT_DEP_URL}", tag = "1.0.11" }}\n')
    (path / "src/lib.rs").write_text('pub fn value() -> u32 { itoa_fixture::value() }\n')
    (path / "Cargo.lock").write_text(f'version = 4\n\n[[package]]\nname = "git-tag-qualification"\nversion = "0.1.0"\ndependencies = ["itoa-fixture"]\n\n[[package]]\nname = "itoa-fixture"\nversion = "1.0.11"\nsource = "git+{GIT_DEP_URL}?tag=1.0.11#{dep_commit}"\n')
    return (*commit_fixture(path), dep_commit)


def no_lockfile_fixture(path):
    path.mkdir()
    (path / "run.sh").write_text('set -eu\ntest ! -e Cargo.lock\ntest ! -e Cargo.toml\nprintf "no-lockfile-started\\n"\n')
    return commit_fixture(path)


def dependency_bundle(repo, bundle, commit):
    ref = f"refs/ck-motor/dep/{commit}"
    git(repo, "update-ref", ref, commit)
    try:
        git(repo, "bundle", "create", str(bundle), ref)
    finally:
        git(repo, "update-ref", "-d", ref)


def run_preparation_case(root, runner, evidence, name, repo, commit, tree, command, dependency=None):
    bundle = evidence / f"{name}.bundle"
    fixture_bundle(repo, bundle, commit)
    payloads = [bundle.read_bytes()]
    workspace = f"/qualification/{name}"
    manifests = [dict(index=0, repository=workspace, length=len(payloads[0]), sha256=hashlib.sha256(payloads[0]).hexdigest())]
    dependencies = []
    if dependency is not None:
        dep_repo, dep_commit = dependency
        dep_bundle = evidence / f"{name}-dependency.bundle"
        dependency_bundle(dep_repo, dep_bundle, dep_commit)
        payloads.append(dep_bundle.read_bytes())
        # A dependency's bundle travels only in its own entry. The runner
        # uploads the run's `bundles` followed by each dependency's bundle, so
        # listing it in `bundles` too would number it twice.
        dependencies.append(dict(url=GIT_DEP_URL, commits=[dep_commit], bundle=dict(index=1, repository=GIT_DEP_URL, length=len(payloads[1]), sha256=hashlib.sha256(payloads[1]).hexdigest())))
    job = str(uuid.uuid4())
    channel = Channel(runner, root, evidence / f"bridge-{name}.log")
    try:
        channel.send(dict(type="run", job_id=job, gate_job=True, platform="windows", workspace_key=workspace, repository_root=workspace, cwd=workspace, command=BUN_VERSION_CHECK + " || exit $?; " + command, env={}, weight_hint=4, timeout=600, queue_wait_limit_s=60, snapshot_commit=commit, tree_hash=tree, integration_tip=commit, siblings=[], git_dependencies=dependencies, bundles=manifests))
        channel.until("accepted")
        for index, data in enumerate(payloads):
            for offset in range(0, len(data), 65536):
                channel.send(dict(type="bundle_chunk", index=index, offset=offset, data=base64.b64encode(data[offset:offset+65536]).decode()))
        output = bytearray()
        terminal = channel.until("terminal", output)
        (evidence / f"terminal-{name}.json").write_text(json.dumps(terminal, indent=2))
        (evidence / f"output-{name}.bin").write_bytes(output)
        require_gate_success(terminal)
        print(f"PASS {name} job={job}", flush=True)
    finally:
        channel.close()


def qualify_preparation_cases(root, runner, evidence):
    repo = evidence / "git-tag-fixture"
    dependency = evidence / "git-tag-dependency"
    commit, tree, dep_commit = dependency_fixture(repo, dependency)
    run_preparation_case(root, runner, evidence, "git-tag-offline", repo, commit, tree,
                         "cargo build --offline --locked", (dependency, dep_commit))
    repo = evidence / "no-lockfile-fixture"
    commit, tree = no_lockfile_fixture(repo)
    run_preparation_case(root, runner, evidence, "no-lockfile", repo, commit, tree, "sh run.sh")


RETURN_QUALIFICATION_BYTES = b"motor requested return qualification"


def returned_file_script(path):
    data = base64.b64encode(RETURN_QUALIFICATION_BYTES).decode("ascii")
    return f"$ErrorActionPreference='Stop'; [IO.File]::WriteAllBytes('{path}',[Convert]::FromBase64String('{data}'))"


RETURN_QUALIFICATION_CASES = (
    ("requested-return-exact-bytes", returned_file_script("returned-file.bin"), "returned-file.bin", None),
    ("requested-return-junction-refused", r"$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path 'return-junction' -Target $env:WINDIR -ErrorAction Stop | Out-Null; if(-not (Test-Path -LiteralPath 'return-junction\win.ini' -PathType Leaf)){throw 'outside junction canary missing'}", "return-junction/win.ini", "escape"),
    ("requested-return-ads-refused", "$ErrorActionPreference='Stop'; [IO.File]::WriteAllText('file.txt','ordinary file'); Set-Content -LiteralPath 'file.txt' -Stream 'hidden' -Value 'ADS must not be returned' -ErrorAction Stop; if((Get-Item -LiteralPath 'file.txt' -Stream 'hidden' -ErrorAction Stop).Length -le 0){throw 'ADS canary missing'}", "file.txt:hidden", "escape"),
    ("no-requested-returns-preserves-changed-source", returned_file_script("returned-legacy.txt"), None, None),
)


def check_requested_return(frames, job, path, cause):
    returned = [frame for frame in frames if frame["type"].startswith("return_")]
    manifests = [frame for frame in returned if frame["type"] == "return_manifest"]
    if len(manifests) != 1 or any(frame["type"] not in ("return_chunk", "return_manifest") for frame in returned):
        raise RuntimeError("requested return did not deliver exactly one chunked manifest")
    manifest = manifests[0]
    if any(frame.get("job_id") != job for frame in returned) or manifest.get("cap_bytes") != 4096:
        raise RuntimeError("requested return has the wrong job or cap")
    files = manifest.get("files")
    if not isinstance(files, list) or len(files) != 1 or files[0].get("path") != path:
        raise RuntimeError("requested return did not echo its path")
    chunks = [frame for frame in returned if frame["type"] == "return_chunk"]
    entry = files[0]
    # The runner states each entry's chunk count, as the exec-remote contract
    # requires of a producer that states it for any entry.
    if entry.get("chunks") != len(chunks):
        raise RuntimeError("requested return manifest chunk count differs from received chunks")
    if cause is not None:
        if chunks or manifest.get("total_bytes") != 0:
            raise RuntimeError("outside or ADS bytes reached the host")
        if entry.get("cause") != cause or set(entry) - {"path", "cause", "size", "chunks"}:
            raise RuntimeError("requested path did not fail with the typed escape cause")
        return manifest
    data = bytearray()
    previous_seq = 0
    for frame in returned:
        if not isinstance(frame.get("seq"), int) or frame["seq"] <= previous_seq:
            raise RuntimeError("requested return sequences are not increasing")
        previous_seq = frame["seq"]
        if frame["type"] == "return_manifest":
            if frame is not returned[-1]:
                raise RuntimeError("requested return delivered chunks after the manifest")
            continue
        chunk = base64.b64decode(frame["data"], validate=True)
        if frame.get("path_index") != 0 or frame.get("offset") != len(data) or not chunk or len(data) + len(chunk) > 4096:
            raise RuntimeError("requested return chunks have an invalid index, offset or size")
        data.extend(chunk)
    digest = hashlib.sha256(RETURN_QUALIFICATION_BYTES).hexdigest()
    if bytes(data) != RETURN_QUALIFICATION_BYTES or hashlib.sha256(data).hexdigest() != digest:
        raise RuntimeError("requested return bytes or SHA-256 differ from the fixture")
    if entry != dict(path=path, size=len(data), sha256=digest, chunks=len(chunks)) or manifest.get("total_bytes") != len(data):
        raise RuntimeError("requested return manifest size or SHA-256 differs from received bytes")
    return manifest


def check_legacy_return(root, job, workspace, frames):
    if any(frame["type"].startswith("return_") for frame in frames):
        raise RuntimeError("job without requested returns delivered returned-file frames")
    member_name = f"m{workspace}/returned-legacy.txt"
    directory = root / "state/ally/jobs" / job
    with tarfile.open(directory / "windows-changes.tar") as archive:
        member = archive.extractfile(member_name)
        if member is None or member.read() != RETURN_QUALIFICATION_BYTES:
            raise RuntimeError("legacy changed-source file is missing or corrupt")
    changes = json.loads((directory / "windows-changes.json").read_text())
    if member_name not in changes:
        raise RuntimeError("legacy changed-source manifest omitted the created file")


def qualify_return_cases(root, runner, evidence):
    repo = evidence / "return-fixture"
    commit, tree = no_lockfile_fixture(repo)
    bundle = evidence / "return-fixture.bundle"
    fixture_bundle(repo, bundle, commit)
    data = bundle.read_bytes()
    for name, script, relative, cause in RETURN_QUALIFICATION_CASES:
        workspace = f"/qualification/{name}"
        requested_path = None if relative is None else f"{workspace}/{relative}"
        job = str(uuid.uuid4())
        channel = Channel(runner, root, evidence / f"bridge-{name}.log")
        try:
            command = "powershell.exe -NoProfile -EncodedCommand " + base64.b64encode(script.encode("utf-16le")).decode("ascii")
            request = dict(type="run", job_id=job, gate_job=True, platform="windows", workspace_key=workspace, repository_root=workspace, cwd=workspace, command=command, env={}, weight_hint=4, timeout=120, queue_wait_limit_s=60, snapshot_commit=commit, tree_hash=tree, integration_tip=commit, siblings=[], git_dependencies=[], bundles=[dict(index=0, repository=workspace, length=len(data), sha256=hashlib.sha256(data).hexdigest())])
            if requested_path is not None:
                request.update(return_paths=[requested_path], return_chunked=True, return_max_bytes=4096)
            channel.send(request)
            channel.until("accepted")
            for offset in range(0, len(data), 65536):
                channel.send(dict(type="bundle_chunk", index=0, offset=offset, data=base64.b64encode(data[offset:offset+65536]).decode()))
            output = bytearray()
            frames = []
            terminal = channel.until("terminal", output, frames=frames)
            require_gate_success(terminal)
            if requested_path is None:
                check_legacy_return(root, job, workspace, frames)
            else:
                manifest = check_requested_return(frames, job, requested_path, cause)
                (evidence / f"return-manifest-{name}.json").write_text(json.dumps(manifest, indent=2))
            (evidence / f"terminal-{name}.json").write_text(json.dumps(terminal, indent=2))
            print(f"PASS {name} job={job}", flush=True)
        finally:
            channel.close()


def run(root, runner, evidence, gate):
    require_runner_service()
    qualify_preparation_cases(root, runner, evidence)
    qualify_return_cases(root, runner, evidence)
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
            command = BUN_VERSION_CHECK + " || exit $?; " + JQ_VERSION_CHECK + " || exit $?; " + CACHE_DRIVE_CHECK + " || exit $?; " + NEXTEST_VERSION_CHECK + " || exit $?; " + PYTHON_CHECK + " || exit $?; " + NATS_VERSION_CHECK + " || exit $?; " + LLVM_CHECK + " || exit $?; " + STABLE_VERSION_CHECK + " || exit $?; " + ("test -f /c/t/qualification-cache-marker && " if iteration else "") + gate + " && printf cached > /c/t/qualification-cache-marker"
            request = dict(type="run", job_id=job, gate_job=True, platform="windows", workspace_key="/qualification/workspace", repository_root="/qualification/workspace", cwd="/qualification/workspace", command=command, env={}, weight_hint=4, timeout=600, queue_wait_limit_s=60, snapshot_commit=commit, tree_hash=tree, integration_tip=commit, siblings=[], git_dependencies=[], bundles=[dict(index=0, repository="/qualification/workspace", length=len(data), sha256=hashlib.sha256(data).hexdigest())])
            channel.send(request)
            channel.until("accepted")
            for offset in range(0, len(data), 65536):
                channel.send(dict(type="bundle_chunk", index=0, offset=offset, data=base64.b64encode(data[offset:offset+65536]).decode()))
            output = bytearray()
            terminal = channel.until("terminal", output)
            (evidence / f"terminal-{iteration}.json").write_text(json.dumps(terminal, indent=2))
            (evidence / f"output-{iteration}.bin").write_bytes(output)
            require_gate_success(terminal)
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
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--wait-ready", action="store_true", help="Wait for service Running and a validated Windows Hello after parent verification")
    mode.add_argument("--preparation-only", action="store_true", help="Run only the tagged offline Git and no-lockfile jobs once each")
    args = parser.parse_args(argv)
    args.evidence.mkdir(parents=True, exist_ok=False)
    if args.wait_ready:
        wait_for_service_ready(args.root, args.root / "bin/motor-runner.exe", args.evidence)
    elif args.preparation_only:
        qualify_preparation_cases(args.root, args.root / "bin/motor-runner.exe", args.evidence)
    else:
        run(args.root, args.root / "bin/motor-runner.exe", args.evidence, args.gate_command)


if __name__ == "__main__":
    main()
