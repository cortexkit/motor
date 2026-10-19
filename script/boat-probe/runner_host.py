"""Real motor jobs in a disposable Boat host; no broker or token endpoint."""
import contextlib
import hashlib
import json
from pathlib import Path
import shlex
import socket
import subprocess
import tempfile
import time

from credential_audit import SOURCE
from ssh_reach import endpoint, pin_host, process, ssh_argv

ROOT = Path(__file__).resolve().parents[2]
PORT = 19443
ENV_NAMES = "import os,json; print(json.dumps({k: k in os.environ for k in ('ASCII_TOKEN','BOAT_ID','BOAT_CLI_TOKEN')}))"
POLICY = {"allowtcpforwarding": "local", "permittty": "no", "allowagentforwarding": "no",
          "x11forwarding": "no", "permittunnel": "no"}
DIAGNOSTICS = r'''
import json, subprocess
out = {}
def command(argv):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=10)
        return {'argv':argv, 'exit_code':result.returncode, 'stdout':result.stdout, 'stderr':result.stderr}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {'argv':argv, 'error':str(error)}
for unit in ('motor-boat.service','motor-boat-host.service','motor-boat-host.path','motor-boat-bridge.service'):
    out[unit] = {'status':command(['/usr/bin/systemctl','status','--no-pager','-l',unit]),
                 'journal':command(['/usr/bin/journalctl','-u',unit,'-b','--no-pager','-n','200'])}
print(json.dumps(out))
'''

JOB = r'''
import json, os, pathlib, pwd, subprocess
cg = pathlib.Path('/sys/fs/cgroup') / pathlib.Path('/proc/self/cgroup').read_text().strip().split('0::',1)[1].lstrip('/')
shm = pathlib.Path('/dev/shm/boat-host-canary')
private = not shm.exists()
probe = pathlib.Path('/dev/shm/job-canary')
probe.write_text('private')
data = {'user': pwd.getpwuid(os.getuid()).pw_name, 'uid': os.getuid(),
        'interfaces': sorted(p.name for p in pathlib.Path('/sys/class/net').iterdir()),
        'namespaces': {n: os.readlink('/proc/self/ns/'+n) for n in ('net','pid','mnt')},
        'private_shm': private, 'shm_writable': probe.read_text() == 'private',
        'nproc': subprocess.check_output(['nproc'], text=True).strip(),
        'cgroup': str(cg), 'limits': {n: (cg/n).read_text().strip() for n in ('memory.max','cpu.max','pids.max')},
        'provider_environment': {k: k in os.environ for k in ('ASCII_TOKEN','BOAT_ID','BOAT_CLI_TOKEN')}}
print(json.dumps(data))
'''
HOST = SOURCE + r'''
import json, os, pathlib, socket, stat, struct, subprocess
pathlib.Path('/dev/shm/boat-host-canary').write_text('not for jobs')
def capture(argv):
    p = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    return {'exit_code': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}
configs = {str(p): p.read_text() for p in pathlib.Path('/etc/ssh/sshd_config.d').glob('*.conf')}
pid = subprocess.check_output(['systemctl','show','motor-boat','--property=MainPID','--value'], text=True).strip()
names = {v.split(b'=',1)[0].decode() for v in pathlib.Path('/proc/'+pid+'/environ').read_bytes().split(b'\0') if b'=' in v}
idle = {}
for unit in ('motor-boat', 'motor-boat-bridge', 'motor-boat-host'):
    group = subprocess.check_output(['systemctl','show',unit,'--property=ControlGroup','--value'], text=True).strip()
    path = pathlib.Path('/sys/fs/cgroup') / group.lstrip('/')
    idle[unit] = {'cgroup': group, 'memory.current': (path/'memory.current').read_text().strip()}
def endpoint_refusal():
    try:
        hello = json.dumps({'type':'lease_hello','v':1,'token':'not-a-valid-token',
            'requester_jwk':{'kty':'OKP','crv':'Ed25519','x':'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'}}).encode()
        with socket.create_connection(('127.0.0.1',9443), timeout=15) as client:
            client.sendall(struct.pack('>I',len(hello))+hello)
            def exact(n):
                out = b''
                while len(out) < n:
                    chunk = client.recv(n-len(out))
                    if not chunk:
                        raise ValueError('endpoint closed without a refusal frame')
                    out += chunk
                return out
            length = struct.unpack('>I',exact(4))[0]
            if length > 1048576:
                raise ValueError('unexpected endpoint frame length')
            return {'connected':True, 'frame':json.loads(exact(length))}
    except (OSError, ValueError) as error:
        return {'error':str(error)}
tenant_sockets = {}
for path in [pathlib.Path('/run/motor-tenant'), *[pathlib.Path('/run/motor-tenant')/name for name in ('jobs.sock','events.sock','admin.sock')]]:
    meta = path.lstat()
    tenant_sockets[str(path)] = {'uid':meta.st_uid, 'mode':oct(stat.S_IMODE(meta.st_mode)), 'socket':stat.S_ISSOCK(meta.st_mode)}
print(json.dumps({'namespaces': {n: os.readlink('/proc/self/ns/'+n) for n in ('net','pid','mnt')},
    'credential_audit': credential_audit(), 'ssh_config': configs,
    'sshd': capture(['/usr/sbin/sshd','-T']),
    'systemd_env': capture(['systemd-run','--wait','--pipe','--collect','python3','-c',ENV_NAMES]),
    'runner_unit': capture(['systemctl','show','motor-boat','--property=MainPID','--property=Environment']),
    'runner_environment': {k: k in names for k in ('ASCII_TOKEN','BOAT_ID','BOAT_CLI_TOKEN')},
    'memory_budget': json.loads(pathlib.Path('/var/lib/motor-boat/budget.json').read_text()),
    'idle_services_memory': idle, 'meminfo_at_idle': pathlib.Path('/proc/meminfo').read_text(),
    'endpoint_refusal': endpoint_refusal(), 'tenant_sockets': tenant_sockets,
    'listeners': capture(['ss','-ltnp']),
    'inventory': json.loads(pathlib.Path('/var/lib/motor-boat/inventory.json').read_text())}))
'''.replace('ENV_NAMES', repr(ENV_NAMES))


def job_result(text):
    result = json.loads(text)
    if type(result.get("code")) is not int or not isinstance(result.get("terminal"), dict):
        raise ValueError("client did not produce a terminal")
    return result


def isolation_checks(job, host):
    cpu = job["limits"]["cpu.max"].split()
    budget = host['memory_budget']
    expected = (budget['mem_available_bytes'] - budget['runner_idle_bytes'] - budget['margin_bytes']) // (1 << 20) * (1 << 20)
    return {
        "job_account": job["user"] == "motor-job" and job["uid"] != 0,
        "loopback_only": job["interfaces"] == ["lo"],
        "private_network": job["namespaces"]["net"] != host["namespaces"]["net"],
        "private_shm": job["private_shm"] is True and job["shm_writable"] is True,
        "memory_limit": job["limits"]["memory.max"] == str(budget['memory_max_bytes']) and budget['memory_max_bytes'] == expected and expected > (1 << 30),
        "cpu_limit": len(cpu) == 2 and cpu == ["800000", "100000"] and job['nproc'] == '8',
        "pid_limit": job["limits"]["pids.max"] == "4096",
        "no_provider_job_environment": not any(job["provider_environment"].values()),
        # This explicit Boat profile does not promise a private PID namespace on systemd 255.
        "boat_pid_profile": job["namespaces"]["pid"] == host["namespaces"]["pid"],
    }


def policy_checks(host):
    actual = dict(line.split(None, 1) for line in host["sshd"]["stdout"].splitlines() if " " in line)
    return {key: host["sshd"].get("exit_code") == 0 and actual.get(key) == value for key, value in POLICY.items()}


def admission_checks(host):
    refusal = host['endpoint_refusal']
    sockets = host['tenant_sockets']
    expected = {'/run/motor-tenant', *('/run/motor-tenant/' + name for name in ('jobs.sock','events.sock','admin.sock'))}
    return {
        'endpoint_rejects_invalid_token': refusal.get('connected') is True and refusal.get('frame') == {'type':'lease_refused','reason':'auth'},
        'root_private_tenant_sockets': set(sockets) == expected and all(meta['uid'] == 0 and
            meta['mode'] == ('0o700' if path == '/run/motor-tenant' else '0o600') and
            (path == '/run/motor-tenant' or meta['socket']) for path,meta in sockets.items()),
    }


@contextlib.contextmanager
def tunnel(argv, directory, deadline, evidence):
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    command = argv[:-1] + ["-o", "ExitOnForwardFailure=yes", "-N", "-L",
                            f"127.0.0.1:{port}:127.0.0.1:{PORT}", argv[-1]]
    log = directory / "tunnel.stderr"
    with log.open("w") as stream:
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stream)
    try:
        until = min(deadline, time.monotonic() + 25)
        while time.monotonic() < until:
            if child.poll() is not None:
                raise ValueError("pinned SSH forward exited: " + log.read_text())
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    break
            except OSError:
                time.sleep(.1)
        else:
            raise ValueError("SSH forward readiness timed out")
        evidence["argv"] = command
        yield f"127.0.0.1:{port}"
    finally:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
        evidence.update(reaped=child.poll() is not None, stderr=log.read_text())


class RunnerHost:
    def __init__(self, probe):
        self.probe = probe
        self.data = {"checks": [], "phases": [], "all_passed": False}
        probe.data["runner_host"] = self.data

    def check(self, name, passed, evidence):
        self.data["checks"].append({"name": name, "status": "PASS" if passed else "FAIL", "evidence": evidence})
        self.probe.checkpoint()

    def execute(self, sid, command, timeout=120):
        result = self.probe.execute(sid, command, poll_seconds=timeout)
        if result.get("exitCode") != 0 or result.get("stdoutTruncated") or result.get("stderrTruncated"):
            failure = {'command': command, 'result': result}
            self.data.setdefault('guest_failures', []).append(failure)
            try:
                diagnostics = self.probe.execute(sid, 'sudo -n python3 -c ' + shlex.quote(DIAGNOSTICS), poll_seconds=100)
                failure['diagnostic_command'] = diagnostics
                if diagnostics.get('exitCode') == 0 and not diagnostics.get('stdoutTruncated'):
                    failure['unit_diagnostics'] = json.loads(diagnostics['stdout'])
            except Exception as error:
                failure['diagnostics_error'] = str(error)
            self.probe.checkpoint()
            raise ValueError("guest command failed or truncated; see runner_host.guest_failures unit_diagnostics: " + json.dumps(result))
        return result

    def route(self, sid, directory, public):
        response = self.probe.request("POST", f"/sandboxes/{sid}/sshkey", {"key": public})
        pin_host(directory / "known_hosts", sid, response)
        self.data.setdefault("ssh_metadata", []).append(response)
        return ssh_argv(endpoint(response), directory / "key", directory / "known_hosts", sid)

    def phase(self, sid, label, directory, public, repository, client, first_started=None):
        record = {"name": label}
        self.data["phases"].append(record)
        identity = "import os; from pathlib import Path; p=Path('/etc/motor/tenant/id'); p.write_text(" + repr(sid) + "); os.chown(p,0,0); p.chmod(0o444)"
        self.execute(sid, "sudo -n python3 -c " + shlex.quote(identity) + " && sudo -n systemctl restart motor-boat motor-boat-host motor-boat-bridge && sudo -n systemctl start motor-boat-host.path")
        argv = self.route(sid, directory, public)
        record["host"] = json.loads(self.execute(sid, "sudo -n -E python3 -c " + shlex.quote(HOST))["stdout"])
        host = record["host"]
        listeners = host["listeners"]["stdout"]
        self.check(label + ".bridge_loopback_only", f"127.0.0.1:{PORT}" in listeners and
                   not any(s in listeners for s in (f"0.0.0.0:{PORT}", f"[::]:{PORT}", f"*:{PORT}")), host["listeners"])
        self.check(label + '.endpoint_loopback_only', '127.0.0.1:9443' in listeners and
                   not any(s in listeners for s in ('0.0.0.0:9443','[::]:9443','*:9443')), host['listeners'])
        for key, passed in admission_checks(host).items():
            self.check(label + '.' + key, passed, {'refusal': host['endpoint_refusal'], 'sockets': host['tenant_sockets']})
        ssh = process(argv + ["python3 -c " + shlex.quote(ENV_NAMES)])
        record["ssh_environment"] = ssh
        self.check(label + ".ssh_environment_measured", ssh.get("exit_code") == 0, ssh)
        for key, passed in policy_checks(host).items():
            self.check(label + ".sshd." + key, passed, host["sshd"])
        self.check(label + ".no_account_credentials", host["credential_audit"]["clean"], host["credential_audit"])
        self.check(label + ".systemd_environment_measured", host["systemd_env"]["exit_code"] == 0, host["systemd_env"])
        self.check(label + ".no_provider_runner_environment", not any(host["runner_environment"].values()), host["runner_environment"])
        with tunnel(argv, directory, self.probe.deadline, record.setdefault("tunnel", {})) as address:
            with tempfile.TemporaryDirectory(prefix="motor-boat-client-") as storage:
                for name, command in [("exit", "printf 'boat-round-trip\\n'; exit 23"),
                                      ("rust", 'cargo +1.99.0 build --offline --locked && "$CARGO_TARGET_DIR/debug/boat-hello"'),
                                      ("isolation", "python3 -c " + shlex.quote(JOB))]:
                    result = process([str(client), address, str(repository), storage, command], timeout=min(150, max(1, int(self.probe.deadline-time.monotonic()))))
                    record[name] = result
                    if result.get("exit_code") != 0:
                        raise ValueError("motor client failed: " + json.dumps(result))
                    parsed = job_result(result["stdout"])
                    record[name]["result"] = parsed
                    if first_started is not None and name == "exit":
                        self.data["snapshot_create_to_first_job_seconds"] = time.monotonic() - first_started
                    if name == "isolation":
                        for key, passed in isolation_checks(json.loads(parsed["stdout"]), host).items():
                            self.check(label + "." + key, parsed["code"] == 0 and passed, parsed)
                    else:
                        expected = 23 if name == "exit" else 0
                        marker = "boat-round-trip" if name == "exit" else "boat-hello"
                        self.check(label + "." + name, parsed["code"] == expected and marker in parsed["stdout"], parsed)
        self.check(label + ".tunnel_reaped", record["tunnel"].get("reaped") is True, record["tunnel"])
        return record

    def run(self):
        self.directory = None
        try:
            self._run()
        finally:
            self.data["local_private_keys_removed"] = self.directory is None or not self.directory.exists()
            self.probe.checkpoint()

    def _run(self):
        from probe import READY, cleanup
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        subprocess.run(["git", "diff", "--exit-code", "HEAD", "--"], cwd=ROOT, check=True)
        build = process(["env", "-u", "GIT_CONFIG_COUNT", "-u", "GIT_CONFIG_KEY_0", "-u", "GIT_CONFIG_VALUE_0",
                         "cargo", "build", "--locked", "-p", "motor-module", "--example", "boat_probe"], timeout=600)
        self.data["client_build"] = build
        if build.get("exit_code") != 0:
            raise ValueError("local operator client build failed")
        client = ROOT / "target/debug/examples/boat_probe"
        # The 30-minute provider budget starts after the local client is available.
        self.probe.deadline = time.monotonic() + min(1800, self.probe.minutes * 60)
        self.data["source_revision"] = revision
        with tempfile.TemporaryDirectory(prefix="motor-boat-ssh-") as temporary:
            directory = Path(temporary)
            self.directory = directory
            generated = process(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(directory / "key")])
            if generated.get("exit_code") != 0:
                raise ValueError("temporary SSH key generation failed")
            public = (directory / "key.pub").read_text().strip()
            item, self.data["create"] = self.probe.create()
            sid = item["id"]
            if item.get("vcpu") != 8 or item.get("memoryGB") != 16:
                raise ValueError("unexpected large sandbox sizing")
            argv = self.route(sid, directory, public)
            bundle = directory / "motor.bundle"
            subprocess.run(["git", "bundle", "create", str(bundle), "HEAD"], cwd=ROOT, check=True)
            sha = hashlib.sha256(bundle.read_bytes()).hexdigest()
            with bundle.open("rb") as stream:
                sent = subprocess.run(argv + ["cat > /home/user/motor.bundle"], stdin=stream, capture_output=True, timeout=120)
            if sent.returncode:
                raise ValueError("source upload over pinned SSH failed: " + sent.stderr.decode(errors="replace"))
            install = (f"cd /home/user && echo '{sha}  motor.bundle' | sha256sum -c - && "
                       "sudo -n mkdir -p /srv/build && sudo -n git clone motor.bundle /srv/build/source && "
                       f"sudo -n git -C /srv/build/source checkout --detach {revision}")
            started = time.monotonic()
            self.data["source_upload"] = self.execute(sid, install)
            self.data["apt_preflight"] = json.loads(self.execute(sid, "sudo -n python3 /srv/build/source/script/boat-image/install.py --apt-preflight-only", timeout=180)["stdout"])
            self.probe.checkpoint()
            self.data["install"] = self.execute(sid, f"sudo -n python3 /srv/build/source/script/boat-image/install.py --revision {revision}", timeout=max(1, int(self.probe.deadline-time.monotonic())-120))
            self.data["install_seconds"] = time.monotonic() - started
            with tempfile.TemporaryDirectory(prefix="boat-job-", dir=ROOT / ".cortexkit") as repo:
                repository = Path(repo)
                (repository / "src").mkdir()
                (repository / "Cargo.toml").write_text('[package]\nname="boat-hello"\nversion="0.1.0"\nedition="2021"\n')
                (repository / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "boat-hello"\nversion = "0.1.0"\n')
                (repository / "src/main.rs").write_text('fn main() { println!("boat-hello"); }\n')
                for command in (["git", "init", "-q"], ["git", "add", "."], ["git", "-c", "user.name=Boat probe", "-c", "user.email=probe@invalid", "commit", "-qm", "probe input"]):
                    subprocess.run(command, cwd=repository, check=True)
                self.phase(sid, "initial", directory, public, repository, client)
                audit = json.loads(self.execute(sid, "sudo -n -E python3 -c " + shlex.quote(SOURCE + "\nimport json; print(json.dumps(credential_audit()))"))["stdout"])
                self.check("snapshot.no_account_credentials", audit["clean"], audit)
                if not audit["clean"]:
                    raise ValueError("refusing to snapshot account credentials or incomplete audit")
                self.execute(sid, "sudo -n systemctl stop motor-boat-host.path motor-boat-host motor-boat-bridge motor-boat && sudo -n rm -f /etc/motor/tenant/id && test ! -e /etc/motor/tenant/id && sudo -n sync && sudo -n umount /Users && sudo -n umount /var/lib/motor-boat && sudo -n sync")
                name = "motor-runner-" + self.probe.data["run_id"]
                self.probe.data["resources"].append({"kind": "named-snapshot", "id": name})
                self.probe.checkpoint()
                start = time.monotonic()
                self.probe.request("POST", "/named-snapshots", {"sandboxId": sid, "name": name})
                while True:
                    snapshot = self.probe.request("GET", "/named-snapshots/" + name)["snapshot"]
                    if snapshot["status"] == "ready":
                        break
                    if snapshot["status"] == "failed":
                        raise ValueError("named snapshot failed")
                    time.sleep(2)
                self.data["snapshot"] = {"seconds": time.monotonic()-start, "state": snapshot, "tenant_id_absent": True, "state_unmounted": True}
                original = next(r for r in self.probe.data["resources"] if r["id"] == sid)
                subset = {"resources": [original], "pending_creates": [], "cleanup": self.probe.data["cleanup"]}
                cleanup(self.probe.client, subset, self.probe.checkpoint)
                state = original.get("deletion_state", {})
                if not original.get("deleted") and state.get("stage") != "waiting_for_uploads":
                    raise ValueError("original is not confirmed stopped/deletion-accepted; refusing a concurrent clone")
                start = time.monotonic()
                clone, self.data["create_from_snapshot"] = self.probe.create(name)
                sid = clone["id"]
                self.probe.wait_hydrated(sid)
                self.phase(sid, "snapshot_clone", directory, public, repository, client, first_started=start)
                self.probe.request("POST", f"/sandboxes/{sid}/stop", {})
                self.probe.wait_sandbox(sid, {"archived"})
                self.probe.request("POST", f"/sandboxes/{sid}/resume", {})
                self.probe.wait_sandbox(sid, READY)
                self.probe.wait_hydrated(sid)
                self.phase(sid, "resumed", directory, public, repository, client)
                before = self.data["phases"][1]["host"]["ssh_config"]
                after = self.data["phases"][2]["host"]["ssh_config"]
                self.check("sshd.dropin_survives_resume", before.get("/etc/ssh/sshd_config.d/00-motor-boat.conf") == after.get("/etc/ssh/sshd_config.d/00-motor-boat.conf") and bool(after.get("/etc/ssh/sshd_config.d/00-motor-boat.conf")), {"before": before, "after": after})
        self.data["local_private_keys_removed"] = not directory.exists()
        self.data["all_passed"] = bool(self.data["checks"]) and all(c["status"] == "PASS" for c in self.data["checks"])
        self.probe.checkpoint()
