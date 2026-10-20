#!/usr/bin/env python3
"""Exercise admission, workspace transfer and an isolated command after a swap."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import uuid


class CanaryError(Exception):
    pass


def send(sock, frame):
    body = json.dumps(frame).encode()
    sock.sendall(struct.pack('!I', len(body)) + body)


def receive(sock):
    def exact(size):
        data = bytearray()
        while len(data) < size:
            chunk = sock.recv(size - len(data))
            if not chunk:
                raise CanaryError('runner closed the canary stream')
            data.extend(chunk)
        return data
    size, = struct.unpack('!I', exact(4))
    if size > 1024 * 1024:
        raise CanaryError('oversized canary response')
    frame = json.loads(exact(size))
    if frame['type'] == 'refusal':
        raise CanaryError(f'canary refused: {frame}')
    return frame


def next_record(sock):
    while True:
        frame = receive(sock)
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


def run(state_root, instance, workspace_root):
    job = str(uuid.uuid4())
    workspace = str(workspace_root / instance / job)
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
        reference = f'refs/ck-motor/{job}/snapshot'
        git('update-ref', reference, commit)
        git('bundle', 'create', 'canary.bundle', reference)
        bundle = (Path(tmp) / 'canary.bundle').read_bytes()
    with connect(state_root / instance / 'runner.sock') as sock:
        send(sock, dict(type='hello', protocol_version=1, started=True,
                        default_branch_tip=True))
        if next_record(sock)['type'] != 'hello':
            raise CanaryError('runner did not negotiate canary protocol')
        send(sock, dict(type='run', job_id=job, workspace_key=workspace,
                        repository_root=repository, cwd=workspace,
                        command='test "$(id -un)" = motor-job', env={},
                        weight_hint=1, timeout=5, queue_wait_limit_s=5,
                        snapshot_commit=commit, tree_hash=tree, integration_tip=commit,
                        siblings=[], bundles=[dict(index=0, repository=repository,
                            length=len(bundle), sha256=hashlib.sha256(bundle).hexdigest())]))
        accepted = next_record(sock)
        if accepted['type'] != 'accepted' or accepted.get('job_id') != job:
            raise CanaryError(f'canary was not accepted: {accepted}')
        send(sock, dict(type='bundle_chunk', index=0, offset=0,
                        data=base64.b64encode(bundle).decode()))
        started = False
        while True:
            frame = next_record(sock)
            if frame['type'] == 'started' and frame.get('job_id') == job:
                started = True
            if frame['type'] == 'terminal':
                if (frame.get('job_id') != job or not started or
                        frame.get('outcome') != dict(type='exit', code=0)):
                    raise CanaryError(f'canary did not start and exit 0: {frame}')
                print(f'isolated deployment canary {job}: started and exited 0')
                return


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-root', type=Path, required=True)
    parser.add_argument('--instance', required=True)
    parser.add_argument('--workspace-root', type=Path, default=Path('/Users/motor-deploy-canary'))
    parser.add_argument('--timeout-s', type=int, default=20)
    args = parser.parse_args()
    if not 1 <= args.timeout_s <= 60:
        parser.error('--timeout-s must be between 1 and 60')

    def timeout(_signum, _frame):
        raise CanaryError('deployment canary deadline exceeded')
    signal.signal(signal.SIGALRM, timeout)
    signal.alarm(args.timeout_s)
    try:
        run(args.state_root, args.instance, args.workspace_root)
    except (CanaryError, OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f'{error}\n')
    finally:
        signal.alarm(0)


if __name__ == '__main__':
    main()
