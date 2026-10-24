#!/usr/bin/env python3
"""Exercise admission, workspace transfer and an isolated command after a swap."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid


class CanaryError(Exception):
    pass


class CapacityRefusal(Exception):
    pass


class CanaryDeadline(CanaryError):
    pass


INCONCLUSIVE = 75
CAPACITY_REASONS = {'runner_full', 'queue_wait_exceeded', 'runner_draining'}


class Progress:
    def __init__(self):
        self.state = 'initial'


class Heartbeats:
    def __init__(self, sock):
        self.sock = sock
        self.next_send = time.monotonic() + 1
        self.queue_deadline = None
        self.withdraw_job = None
        self.withdraw_sent = False

    def observe_queue_wait(self, job, limit, elapsed):
        self.withdraw_job = job
        self.queue_deadline = (None if elapsed is None else
                               time.monotonic() + max(0, limit - elapsed / 1000))

    def wait_for_readable(self):
        while True:
            now = time.monotonic()
            if now >= self.next_send:
                send(self.sock, dict(type='heartbeat'))
                self.next_send = now + 1
            if (not self.withdraw_sent and self.queue_deadline is not None and
                    now >= self.queue_deadline):
                send(self.sock, dict(type='withdraw', job_id=self.withdraw_job))
                self.withdraw_sent = True
                self.queue_deadline = None
            # Keep the cadence even when the runner sends frequent heartbeats
            # or a response arrives in several partial reads.
            wake = self.next_send
            if not self.withdraw_sent and self.queue_deadline is not None:
                wake = min(wake, self.queue_deadline)
            if select.select([self.sock], [], [], max(0, wake - now))[0]:
                return


def send(sock, frame):
    body = json.dumps(frame).encode()
    sock.sendall(struct.pack('!I', len(body)) + body)


def receive(sock, heartbeats=None):
    def exact(size):
        data = bytearray()
        while len(data) < size:
            if heartbeats is not None:
                heartbeats.wait_for_readable()
            chunk = sock.recv(size - len(data))
            if not chunk:
                raise CanaryError('runner closed the canary stream')
            data.extend(chunk)
        return data
    size, = struct.unpack('!I', exact(4))
    if size > 1024 * 1024:
        raise CanaryError('oversized canary response')
    frame = json.loads(exact(size))
    return frame


def next_record(sock, heartbeats=None):
    while True:
        frame = receive(sock, heartbeats)
        if frame['type'] != 'heartbeat':
            return frame


def connect(path):
    # systemctl start can return before the daemon binds its socket. The process
    # alarm bounds this retry and all subsequent transfer and execution work.
    while True:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(str(path))
            return sock
        except (FileNotFoundError, ConnectionRefusedError):
            sock.close()
            time.sleep(0.1)


def check_refusal(frame, job, started=False):
    if frame['type'] == 'refusal':
        reason = frame.get('reason')
    elif (frame['type'] == 'terminal' and
          frame.get('outcome', {}).get('type') == 'refused_before_start'):
        reason = frame['outcome'].get('reason')
    else:
        return
    if frame.get('job_id', job) != job:
        raise CanaryError(f'canary response has the wrong job ID: {frame}')
    if not started and reason in CAPACITY_REASONS:
        raise CapacityRefusal(reason)
    raise CanaryError(f'canary refused: {frame}')


def attempt(state_root, instance, workspace, repository, commit, tree, bundle, queue_wait, progress):
    job = str(uuid.uuid4())
    with connect(state_root / instance / 'runner.sock') as sock:
        heartbeats = Heartbeats(sock)
        send(sock, dict(type='hello', protocol_version=1, started=True, gate_jobs=True))
        hello = next_record(sock, heartbeats)
        if hello['type'] != 'hello':
            raise CanaryError('runner did not negotiate canary protocol')
        send(sock, dict(type='run', job_id=job, workspace_key=workspace,
                        repository_root=repository, cwd=workspace,
                        command='test "$(id -un)" = motor-job', env={},
                        weight_hint=1, timeout=5, queue_wait_limit_s=queue_wait,
                        snapshot_commit=commit, tree_hash=tree, integration_tip=commit,
                        siblings=[], bundles=[dict(index=0, repository=repository,
                            length=len(bundle), sha256=hashlib.sha256(bundle).hexdigest())],
                        **({'gate_job': True} if hello.get('gate_jobs') else {})))
        accepted = next_record(sock, heartbeats)
        check_refusal(accepted, job)
        if accepted['type'] != 'accepted' or accepted.get('job_id') != job:
            raise CanaryError(f'canary was not accepted: {accepted}')
        send(sock, dict(type='bundle_chunk', index=0, offset=0,
                        data=base64.b64encode(bundle).decode()))
        progress.state = 'queued'
        started = False
        while True:
            frame = receive(sock, heartbeats)
            if frame['type'] == 'heartbeat':
                if not started:
                    # Queue budgets are client-driven. When a heartbeat omits
                    # queue-wait duration, clear the local withdrawal timer.
                    heartbeats.observe_queue_wait(job, queue_wait, frame.get('queue_wait_ms'))
                continue
            check_refusal(frame, job, started)
            if frame['type'] == 'started' and frame.get('job_id') == job:
                started = True
                progress.state = 'started'
                heartbeats.queue_deadline = None
            if frame['type'] == 'terminal':
                if (frame.get('job_id') != job or not started or
                        frame.get('outcome') != dict(type='exit', code=0)):
                    raise CanaryError(f'canary did not start and exit 0: {frame}')
                print(f'isolated deployment canary {job}: started and exited 0')
                return


def run(state_root, instance, workspace_root, deadline, progress):
    workspace = str(workspace_root / instance / 'workspace')
    repository = workspace + '-source'
    with tempfile.TemporaryDirectory(prefix='motor-deploy-canary-') as tmp:
        def git(*args):
            return subprocess.check_output(
                ['git', '-C', tmp, *args], stderr=subprocess.PIPE).decode().strip()
        git('init', '--quiet')
        git('-c', 'user.name=Motor canary', '-c', 'user.email=canary@example.invalid',
            '-c', 'commit.gpgsign=false', 'commit', '--quiet', '--allow-empty', '-m', 'canary')
        commit = git('rev-parse', 'HEAD')
        tree = git('rev-parse', 'HEAD^{tree}')
        reference = f'refs/ck-motor/{uuid.uuid4()}/snapshot'
        git('update-ref', reference, commit)
        git('bundle', 'create', 'canary.bundle', reference)
        bundle = (Path(tmp) / 'canary.bundle').read_bytes()
    while True:
        remaining = deadline - time.monotonic()
        if progress.state == 'capacity' and remaining <= 1:
            time.sleep(max(0, remaining))
            return INCONCLUSIVE
        progress.state = 'pending'
        try:
            # Leave time for a queue refusal to arrive before the overall alarm.
            queue_wait = max(1, min(10, int(remaining / 2)))
            attempt(state_root, instance, workspace, repository, commit, tree, bundle, queue_wait, progress)
            return 0
        except CapacityRefusal as error:
            progress.state = 'capacity'
            print(f'canary waiting for a free permit: {error}', file=sys.stderr)
            time.sleep(min(1, max(0, deadline - time.monotonic())))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-root', type=Path, required=True)
    parser.add_argument('--instance', required=True)
    parser.add_argument('--workspace-root', type=Path, default=Path('/Users/motor-deploy-canary'))
    parser.add_argument('--timeout-s', type=int, default=120)
    args = parser.parse_args()
    if not 1 <= args.timeout_s <= 3600:
        parser.error('--timeout-s must be between 1 and 3600')

    def timeout(_signum, _frame):
        raise CanaryDeadline('deployment canary deadline exceeded without a terminal result')
    signal.signal(signal.SIGALRM, timeout)
    signal.alarm(args.timeout_s)
    progress = Progress()
    try:
        result = run(args.state_root, args.instance, args.workspace_root,
                     time.monotonic() + args.timeout_s, progress)
    except CanaryDeadline as error:
        # Only a queued job or a capacity refusal can excuse a deadline. A
        # failed handshake, upload, or a started job without a result cannot.
        if progress.state not in ('capacity', 'queued'):
            parser.exit(1, f'{error}\n')
        result = INCONCLUSIVE
    except (CanaryError, OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f'{error}\n')
    finally:
        signal.alarm(0)
    if result == INCONCLUSIVE:
        print('canary inconclusive: no free permit; keeping the new runner (isolation not verified)',
              file=sys.stderr)
    return result


if __name__ == '__main__':
    sys.exit(main())
