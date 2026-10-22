"""Pinned SSH and loopback TLS measurements, controlled from the operator's Mac."""
from __future__ import annotations

import base64
import ipaddress
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import tempfile
import time

from reach import tcp_probe
from probe import provider_sizing_matches

RESTRICTION = 'restrict,port-forwarding,permitopen="127.0.0.1:9443",command="/bin/false"'
OTHER_PORT = 9444

AUTHORIZED_KEYS = r'''
import base64, json, os, pathlib, shlex, sys
public, mode, restriction = sys.argv[1:]
kind, blob = public.split()[:2]
path = pathlib.Path.home() / '.ssh/authorized_keys'
lines = path.read_bytes().splitlines(keepends=True)
def body(line):
    return line.removesuffix(b'\n').removesuffix(b'\r').decode()
def ours(line):
    try:
        fields = shlex.split(body(line))
        index = fields.index(kind)
        return fields[index + 1] == blob
    except (ValueError, IndexError):
        return False
before = [line for line in lines if ours(line)]
if mode == 'restrict':
    if not before:
        raise RuntimeError('tested public key is absent from authorized_keys')
    replacement = (restriction + ' ' + public + '\n').encode()
    lines = [line for line in lines if not ours(line)]
    if lines and not lines[-1].endswith((b'\n', b'\r')):
        lines[-1] += b'\n'
    lines.append(replacement)
    temporary = path.with_name('authorized_keys.boat-probe')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(b''.join(lines))
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    path.chmod(0o600)
after = [line for line in lines if ours(line)]
print(json.dumps({'before': [body(line) for line in before], 'after': [body(line) for line in after],
                  'raw_after': [base64.b64encode(line).decode('ascii') for line in after],
                  'matching_keys': len(after)}))
'''


def process(argv, timeout=20, input_text=None):
    start = time.monotonic()
    result = {"argv": argv}
    environment = {k: v for k, v in os.environ.items() if k != "BOAT_API_KEY"}
    try:
        completed = subprocess.run(argv, input=input_text, stdin=subprocess.DEVNULL if input_text is None else None,
                                   capture_output=True, text=True, timeout=timeout, env=environment)
        result.update(exit_code=completed.returncode, stdout=completed.stdout, stderr=completed.stderr)
    except (OSError, subprocess.TimeoutExpired) as error:
        result["error"] = str(error)
        # Keep what a timed-out command printed before it was killed: a client
        # that never finishes still shows how far it got.
        for name in ("stdout", "stderr"):
            partial = getattr(error, name, None)
            if partial is not None:
                result["partial_" + name] = partial.decode(errors="replace") if isinstance(partial, bytes) else partial
    result["seconds"] = time.monotonic() - start
    return result


def endpoint(response):
    advertised = response.get("sshEndpoint")
    if advertised:
        host, separator, port = advertised.rpartition(":")
        if not separator:
            raise ValueError("sshEndpoint is not host:port")
        host, port = host.strip("[]"), int(port)
    else:
        host, port = response.get("machineIp"), 22
    address = ipaddress.ip_address(host)
    if not 1 <= port <= 65535:
        raise ValueError("SSH port is out of range")
    return {"host": str(address), "port": port, "family": address.version,
            "sshEndpoint": advertised, "machineIp": response.get("machineIp")}


def pin_host(path, alias, response):
    fields = response.get("hostKey", "").split()
    if len(fields) < 2 or "\n" in response.get("hostKey", "") or "\n" in alias:
        raise ValueError("missing or multiline API hostKey")
    if fields[0] not in {"ssh-ed25519", "ssh-rsa", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"}:
        raise ValueError("unrecognized API host key type")
    base64.b64decode(fields[1], validate=True)
    path.write_text(alias + " " + " ".join(fields[:2]) + "\n")
    path.chmod(0o600)


def ssh_argv(route, private, known_hosts, alias):
    options = {"BatchMode": "yes", "StrictHostKeyChecking": "yes", "UserKnownHostsFile": str(known_hosts),
               "GlobalKnownHostsFile": "/dev/null", "HostKeyAlias": alias, "UpdateHostKeys": "no",
               "VerifyHostKeyDNS": "no", "IdentitiesOnly": "yes", "IdentityAgent": "none",
               "PasswordAuthentication": "no", "KbdInteractiveAuthentication": "no",
               "PreferredAuthentications": "publickey", "ConnectTimeout": "8", "ConnectionAttempts": "1",
               "ControlMaster": "no", "ControlPath": "none", "ControlPersist": "no", "LogLevel": "VERBOSE"}
    argv = ["ssh", "-4", "-F", "/dev/null", "-T", "-p", str(route["port"]), "-i", str(private)]
    for name, value in options.items():
        argv += ["-o", name + "=" + value]
    return argv + ["user@" + route["host"]]


def authenticated(result):
    return any(("Authenticated to " in line and "publickey" in line) or
               "Authentication succeeded (publickey)" in line for line in result.get("stderr", "").splitlines())


def refused(result, marker):
    code = result.get("exit_code")
    channel_refused = any(text in result.get("stderr", "") for text in
                          ("exec request failed", "shell request failed", "PTY allocation request failed",
                           "administratively prohibited"))
    return (authenticated(result) and code not in {None, 0} and (code != 255 or channel_refused) and
            marker not in result.get("stdout", ""))


def remote_forward_refused(result):
    return (authenticated(result) and result.get("exit_code") not in {None, 0} and
            "remote port forwarding failed" in result.get("stderr", ""))


def forward(argv, certificate, directory, remote_port=9443):
    """Forward without a session; the base argv ends with the SSH destination."""
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    command = argv[:-1] + ["-o", "ExitOnForwardFailure=yes", "-N", "-L",
                            f"127.0.0.1:{port}:127.0.0.1:{remote_port}", argv[-1]]
    start = time.monotonic()
    result = {"argv": command, "local_port": port, "attempts": []}
    child = None
    log = directory / "forward.stderr"
    environment = {k: v for k, v in os.environ.items() if k != "BOAT_API_KEY"}
    try:
        with log.open("w") as stream:
            child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                     stderr=stream, env=environment)
            until = time.monotonic() + 20
            while time.monotonic() < until and child.poll() is None:
                attempt = tcp_probe("127.0.0.1", port, certificate, timeout=2)
                result["attempts"].append(attempt)
                if attempt.get("verified_echo"):
                    result["verified_echo"] = True
                    result["seconds_to_echo"] = time.monotonic() - start
                    break
                time.sleep(0.2)
    except OSError as error:
        result["error"] = str(error)
    finally:
        if child:
            child.terminate()
            reaped = False
            try:
                child.wait(timeout=5)
                reaped = True
            except subprocess.TimeoutExpired:
                child.kill()
                try:
                    child.wait(timeout=5)
                    reaped = True
                except subprocess.TimeoutExpired:
                    result["reap_error"] = "SSH did not exit after SIGKILL"
            result["tunnel_process_reaped"] = reaped
        result["stderr"] = log.read_text() if log.exists() else ""
        result["seconds"] = time.monotonic() - start
    return result


def blocked_forward(argv, certificate, directory, remote_port):
    """Ask for a disallowed direct-tcpip destination and require sshd's refusal."""
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    command = argv[:-1] + ["-o", "ExitOnForwardFailure=yes", "-N", "-L",
                           f"127.0.0.1:{port}:127.0.0.1:{remote_port}", argv[-1]]
    start = time.monotonic()
    result = {"argv": command, "local_port": port, "remote_port": remote_port, "attempts": []}
    child = None
    log = directory / "blocked-forward.stderr"
    environment = {k: v for k, v in os.environ.items() if k != "BOAT_API_KEY"}
    try:
        with log.open("w") as stream:
            child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                     stderr=stream, env=environment)
            until = time.monotonic() + 8
            while time.monotonic() < until and child.poll() is None:
                attempt = tcp_probe("127.0.0.1", port, certificate, timeout=2)
                result["attempts"].append(attempt)
                if attempt.get("verified_echo"):
                    result["verified_echo"] = True
                    break
                log_text = log.read_text()
                if "administratively prohibited" in log_text:
                    result["server_refused"] = True
                    break
                time.sleep(0.2)
    except OSError as error:
        result["error"] = str(error)
    finally:
        if child:
            child.terminate()
            reaped = False
            try:
                child.wait(timeout=5)
                reaped = True
            except subprocess.TimeoutExpired:
                child.kill()
                try:
                    child.wait(timeout=5)
                    reaped = True
                except subprocess.TimeoutExpired:
                    result["reap_error"] = "SSH did not exit after SIGKILL"
            result["tunnel_process_reaped"] = reaped
        result["stderr"] = log.read_text() if log.exists() else ""
        result["authenticated_publickey"] = authenticated(result)
        result["server_refused"] = (result.get("server_refused") is True or
                                    "administratively prohibited" in result["stderr"])
        result["seconds"] = time.monotonic() - start
    return result


def remote_forward(argv, directory):
    """Request a remote listener; a refused request exits, an accepted one stays alive."""
    command = argv[:-1] + ["-o", "ExitOnForwardFailure=yes", "-N", "-R",
                           "127.0.0.1:0:127.0.0.1:9443", argv[-1]]
    start = time.monotonic()
    result = {"argv": command}
    log = directory / "remote-forward.stderr"
    environment = {k: v for k, v in os.environ.items() if k != "BOAT_API_KEY"}
    child = None
    try:
        with log.open("w") as stream:
            child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                     stderr=stream, env=environment)
            until = time.monotonic() + 8
            while time.monotonic() < until and child.poll() is None:
                time.sleep(0.1)
            if child.poll() is None:
                result["no_refusal_before_timeout"] = True
                child.terminate()
            child.wait(timeout=5)
            result["exit_code"] = child.returncode
    except (OSError, subprocess.TimeoutExpired) as error:
        result["error"] = str(error)
        if child and child.poll() is None:
            child.kill()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                result["reap_error"] = "SSH did not exit after SIGKILL"
    finally:
        result["stderr"] = log.read_text() if log.exists() else ""
        result["authenticated_publickey"] = authenticated(result)
        result["refused"] = remote_forward_refused(result)
        result["tunnel_process_reaped"] = child is None or child.poll() is not None
        result["seconds"] = time.monotonic() - start
    return result


class SSHReach:
    def __init__(self, runner):
        self.runner = runner
        self.data = {"checks": [], "phases": [], "all_passed": False}
        runner.data["ssh_reach"] = self.data
        self.marker = "boat-ssh-" + runner.data["run_id"]

    def check(self, name, passed, evidence):
        self.data["checks"].append({"name": name, "status": "PASS" if passed else "FAIL", "evidence": evidence})
        self.runner.checkpoint()

    def authorize(self, sid, public):
        for attempt in range(10):
            try:
                return self.runner.request("POST", f"/sandboxes/{sid}/sshkey", {"key": public})
            except Exception as error:
                text = str(error)
                if attempt == 9 or not any(word in text for word in ("sandbox_restoring", "sandbox_securing", "sandbox_starting")):
                    raise
                time.sleep(2)
        raise RuntimeError("no SSH authorization response")

    def key_lines(self, sid, public, mode="inspect"):
        text = ("python3 -c " + shlex.quote(AUTHORIZED_KEYS) + " " + shlex.quote(public) + " " + mode +
                " " + shlex.quote(RESTRICTION))
        result = self.runner.execute(sid, text)
        if result.get("exitCode") != 0 or result.get("stdoutTruncated"):
            raise RuntimeError("authorized_keys command failed: " + json.dumps(result))
        return json.loads(result["stdout"])

    def connection(self, argv, name, refuse=False, shell=False, pty=False):
        command = "printf '%s\\n' " + shlex.quote(self.marker)
        shell_argv = argv[:-1] + (["-tt"] if pty else []) + [argv[-1]] if shell else argv + [command]
        if shell:
            midpoint = len(self.marker) // 2
            command = "printf '%s%s\\n' " + shlex.quote(self.marker[:midpoint]) + " " + shlex.quote(self.marker[midpoint:])
        result = process(shell_argv, input_text=command + "\nexit\n" if shell else None)
        result["authenticated_publickey"] = authenticated(result)
        passed = refused(result, self.marker) if refuse else result.get("exit_code") == 0 and self.marker in result.get("stdout", "")
        self.check(name, passed, result)
        return result

    def phase(self, sid, label, response, private, directory, restricted=False):
        known = directory / "known_hosts"
        try:
            route = endpoint(response)
            pin_host(known, sid, response)
        except ValueError as error:
            self.check(label + ".api_pinned_host_key", False, {"error": str(error)})
            raise
        self.check(label + ".api_pinned_host_key", True, {"hostKey": response["hostKey"], "route": route})
        phase = {"name": label, "route": route, "hostKey": response["hostKey"]}
        self.data["phases"].append(phase)
        self.check(label + ".ipv4_endpoint", route["family"] == 4, route)
        argv = ssh_argv(route, private, known, sid)
        phase["command"] = self.connection(argv, label + (".exec_refused" if restricted else ".command"), refuse=restricted)
        if restricted:
            phase["shell_without_pty"] = self.connection(argv, label + ".shell_without_pty_refused", refuse=True, shell=True)
            phase["shell_with_pty"] = self.connection(argv, label + ".shell_with_pty_refused", refuse=True,
                                                       shell=True, pty=True)
        listener = self.runner.guest(sid, "listener-loopback")
        phase["listener"] = listener
        self.check(label + ".loopback_listener", listener.get("ready") is True and
                   listener.get("listening_addresses") == ["127.0.0.1:9443"], listener)
        if listener.get("ready"):
            phase["forward"] = forward(argv, listener["certificate_pem"], directory)
            self.check(label + ".tls_over_permitted_forward", phase["forward"].get("verified_echo") is True, phase["forward"])
            self.check(label + ".tunnel_reaped", phase["forward"].get("tunnel_process_reaped") is True,
                       {"tunnel_process_reaped": phase["forward"].get("tunnel_process_reaped"),
                        "reap_error": phase["forward"].get("reap_error")})
            if restricted:
                other = self.runner.guest(sid, "listener-loopback", port=OTHER_PORT)
                phase["other_destination_listener"] = other
                self.check(label + ".other_destination_listener",
                           other.get("ready") is True and other.get("listening_addresses") == [f"127.0.0.1:{OTHER_PORT}"],
                           other)
                if other.get("ready"):
                    phase["other_destination_forward"] = blocked_forward(
                        argv, other["certificate_pem"], directory, OTHER_PORT)
                    self.check(label + ".other_destination_forward_refused",
                               phase["other_destination_forward"].get("server_refused") is True and
                               phase["other_destination_forward"].get("verified_echo") is not True,
                               phase["other_destination_forward"])
                    self.check(label + ".other_destination_tunnel_reaped",
                               phase["other_destination_forward"].get("tunnel_process_reaped") is True,
                               phase["other_destination_forward"])
                else:
                    self.check(label + ".other_destination_forward_refused", False,
                               {"error": "other-destination listener is not ready"})
                phase["remote_forward"] = remote_forward(argv, directory)
                self.check(label + ".remote_forward_refused",
                           phase["remote_forward"].get("refused") is True, phase["remote_forward"])
                self.check(label + ".remote_forward_tunnel_reaped",
                           phase["remote_forward"].get("tunnel_process_reaped") is True,
                           phase["remote_forward"])
        else:
            self.check(label + ".tls_over_permitted_forward", False, {"error": "loopback listener not ready"})
            if restricted:
                self.check(label + ".other_destination_forward_refused", False,
                           {"error": "permitted loopback listener not ready"})
                self.check(label + ".remote_forward_refused", False,
                           {"error": "permitted loopback listener not ready"})
        return phase

    def resume(self, sid, label):
        start = time.monotonic()
        self.runner.request("POST", f"/sandboxes/{sid}/stop", {})
        stopped = self.runner.wait_sandbox(sid, {"archived"})
        stop_seconds = time.monotonic() - start
        start = time.monotonic()
        self.runner.request("POST", f"/sandboxes/{sid}/resume", {})
        ready = self.runner.wait_sandbox(sid, {"ready", "idle", "running"})
        ready_seconds = time.monotonic() - start
        hydrated = self.runner.wait_hydrated(sid)
        record = {"stopped": stopped, "ready": ready, "hydrated": hydrated, "stop_seconds": stop_seconds,
                  "resume_seconds": ready_seconds, "resume_to_hydrated_seconds": time.monotonic() - start}
        self.check(label + ".stop_resume", True, record)
        return record

    def run(self):
        self.data["ssh_version"] = process(["ssh", "-V"])
        item, self.runner.data["create"] = self.runner.create()
        sid = item["id"]
        self.check("create_" + self.runner.size, provider_sizing_matches(item, self.runner.size), item)
        temporary = tempfile.TemporaryDirectory(prefix="boat-ssh-probe-")
        directory = Path(temporary.name)
        directory.chmod(0o700)
        try:
            private = directory / "probe_ed25519"
            witness = directory / "witness_ed25519"
            for label, key in (("tested", private), ("witness", witness)):
                generated = process(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "boat-probe-" + label, "-f", str(key)])
                self.check(label + ".key_generated", generated.get("exit_code") == 0, generated)
                if generated.get("exit_code") != 0:
                    raise RuntimeError("throwaway SSH key generation failed")
                key.chmod(0o600)
            public = private.with_suffix(".pub").read_text().strip()
            witness_public = witness.with_suffix(".pub").read_text().strip()
            initial = self.authorize(sid, public)
            self.phase(sid, "initial", initial, private, directory)
            self.resume(sid, "plain_resume")
            # Refresh pinning with a different key so the tested line is not reinstalled before its persistence check.
            refreshed = self.authorize(sid, witness_public)
            persisted = self.key_lines(sid, public)
            self.check("old_key_line_after_resume", persisted.get("matching_keys", 0) > 0, persisted)
            self.phase(sid, "old_key_before_reauthorize", refreshed, private, directory)
            reauthorized = self.authorize(sid, public)
            self.phase(sid, "reauthorized", reauthorized, private, directory)
            self.check("resume_metadata_compared", True,
                       {"host_key_changed": initial["hostKey"] != refreshed["hostKey"],
                        "ssh_endpoint_changed": initial.get("sshEndpoint") != refreshed.get("sshEndpoint"),
                        "machine_ip_changed": initial.get("machineIp") != refreshed.get("machineIp")})
            restricted = self.key_lines(sid, public, "restrict")
            expected_line = RESTRICTION + " " + public
            expected_raw_line = base64.b64encode((expected_line + "\n").encode()).decode("ascii")
            self.check("restriction_written", restricted.get("after") == [expected_line], restricted)
            self.check("restriction_written_byte_exact", restricted.get("raw_after") == [expected_raw_line], restricted)
            self.phase(sid, "restricted", reauthorized, private, directory, restricted=True)
            self.resume(sid, "restricted_resume")
            # Reauthorizing the tested key here could remove its options and invalidate the survival test.
            after = self.authorize(sid, witness_public)
            lines = self.key_lines(sid, public)
            self.check("restriction_survived_resume", lines.get("after") == [expected_line], lines)
            self.check("restriction_survived_resume_byte_exact", lines.get("raw_after") == [expected_raw_line], lines)
            self.check("restricted_resume_metadata_compared", True,
                       {"host_key_changed": reauthorized["hostKey"] != after["hostKey"],
                        "ssh_endpoint_changed": reauthorized.get("sshEndpoint") != after.get("sshEndpoint"),
                        "machine_ip_changed": reauthorized.get("machineIp") != after.get("machineIp")})
            self.phase(sid, "restricted_after_resume", after, private, directory, restricted=True)
        finally:
            temporary.cleanup()
            self.data["local_private_keys_removed"] = not directory.exists()
            self.data["all_passed"] = all(c["status"] == "PASS" for c in self.data["checks"])
            self.runner.checkpoint()
