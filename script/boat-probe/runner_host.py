"""Real motor jobs in a disposable Boat host. The admission endpoint is started
the way the production broker does it (identity install, then polling the
readiness marker) in each order the broker can produce. The endpoint only
trusts the production broker's signing keys, so no token it accepts is used."""
import contextlib
import base64
import errno
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import tarfile
import tempfile
import time

from credential_audit import SOURCE
from ssh_reach import endpoint, pin_host, process, ssh_argv

ROOT = Path(__file__).resolve().parents[2]
provider_spec = importlib.util.spec_from_file_location('boat_provider_access', ROOT / 'script/boat-image/motor-boat-credentials.py')
if provider_spec is None or provider_spec.loader is None:
    raise ImportError('cannot load provider access probe')
provider = importlib.util.module_from_spec(provider_spec)
provider_spec.loader.exec_module(provider)
PORT = 19443
ENV_NAMES = "import os,json; print(json.dumps({k: True for k in os.environ if k == 'ASCII_TOKEN' or k.startswith('BOAT_')}))"
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
for unit in ('motor-boat-restored.service','motor-boat-storage.service',r'var-lib-motor\x2dboat.mount','Users.mount','motor-boat.service','motor-boat-host.service','motor-boat-identity.service','motor-boat-bridge.service'):
    out[unit] = {'status':command(['/usr/bin/systemctl','status','--no-pager','-l',unit]),
                 'journal':command(['/usr/bin/journalctl','-u',unit,'-b','--no-pager','-n','200'])}
print(json.dumps(out))
'''

JOB = provider.ACCESS_SOURCE + r'''
import errno, json, os, pathlib, pwd, subprocess
cg = pathlib.Path('/sys/fs/cgroup') / pathlib.Path('/proc/self/cgroup').read_text().strip().split('0::',1)[1].lstrip('/')
shm = pathlib.Path('/dev/shm/boat-host-canary')
private = not shm.exists()
probe = pathlib.Path('/dev/shm/job-canary')
probe.write_text('private')
membership = pathlib.Path('/proc/self/cgroup').read_text()
visible = []
for directory in pathlib.Path('/proc').iterdir():
    if directory.name.isdecimal():
        try:
            status = (directory/'status').read_text()
            uid = int(next(line.split()[1] for line in status.splitlines() if line.startswith('Uid:')))
            visible.append({'pid':int(directory.name), 'uid':uid, 'same_cgroup':(directory/'cgroup').read_text() == membership})
        except FileNotFoundError:
            pass
        except OSError as error:
            visible.append({'pid':int(directory.name), 'inspection_error':type(error).__name__})
ps = subprocess.run(['ps','-eo','pid=,uid=,comm='], capture_output=True, text=True, timeout=10)
ps_rows = [{'pid':int(parts[0]),'uid':int(parts[1]),'comm':parts[2]} for line in ps.stdout.splitlines() if len(parts := line.split(None,2)) == 3]
try:
    os.kill(RUNNER_PID, 0)
    signal_probe = {'pid':RUNNER_PID, 'allowed':True}
except OSError as error:
    signal_probe = {'pid':RUNNER_PID, 'allowed':False, 'errno':error.errno}
data = {'user': pwd.getpwuid(os.getuid()).pw_name, 'uid': os.getuid(),
        'interfaces': sorted(p.name for p in pathlib.Path('/sys/class/net').iterdir()),
        'namespaces': {n: os.readlink('/proc/self/ns/'+n) for n in ('net','pid','mnt')},
        'private_shm': private, 'shm_writable': probe.read_text() == 'private',
        'nproc': subprocess.check_output(['nproc'], text=True).strip(),
        'cgroup': str(cg), 'limits': {n: (cg/n).read_text().strip() for n in ('memory.max','cpu.max','pids.max')},
        'provider_environment': {k: True for k in os.environ if k == 'ASCII_TOKEN' or k.startswith('BOAT_')},
        'provider_access': provider_access(PROVIDER_PATHS, PROVIDER_PIDS)}
data['process_visibility'] = {'proc':visible, 'ps':{'exit_code':ps.returncode,'rows':ps_rows,'stderr':ps.stderr},
    'protected_pids':PROTECTED_PIDS, 'runner_signal_probe':signal_probe, 'self_pid':os.getpid()}
status = pathlib.Path('/proc/self/status').read_text()
data['status_security'] = {key:next(line.split()[1] for line in status.splitlines() if line.startswith(key+':')) for key in ('NoNewPrivs','CapEff','CapBnd')}
data['proc_mountinfo'] = [line for line in pathlib.Path('/proc/self/mountinfo').read_text().splitlines() if line.split()[4] == '/proc']
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
    main_pid = subprocess.check_output(['systemctl','show',unit,'--property=MainPID','--value'], text=True).strip()
    idle[unit]['main_pid'] = int(main_pid)
    unit_names = {v.split(b'=',1)[0].decode() for v in pathlib.Path('/proc/'+main_pid+'/environ').read_bytes().split(b'\0') if b'=' in v}
    idle[unit]['provider_environment_names'] = sorted(k for k in unit_names if k == 'ASCII_TOKEN' or k.startswith('BOAT_'))
sshd_pids = []
for path in pathlib.Path('/proc').iterdir():
    if path.name.isdecimal():
        try:
            if (path/'comm').read_text().strip().startswith('sshd'):
                sshd_pids.append(int(path.name))
        except OSError:
            pass
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
    'runner_environment': {k: True for k in names if k == 'ASCII_TOKEN' or k.startswith('BOAT_')},
    'memory_budget': json.loads(pathlib.Path('/var/lib/motor-boat/budget.json').read_text()),
    'idle_services_memory': idle, 'meminfo_at_idle': pathlib.Path('/proc/meminfo').read_text(),
    'endpoint_refusal': endpoint_refusal(), 'tenant_sockets': tenant_sockets,
    'runner_pid':int(pid),
    'sshd_pids':sshd_pids,
    'provider_sanitization': json.loads(pathlib.Path('/run/motor-boat-credentials.json').read_text()),
    'listeners': capture(['ss','-ltnp']),
    'inventory': json.loads(pathlib.Path('/var/lib/motor-boat/inventory.json').read_text())}))
'''.replace('ENV_NAMES', repr(ENV_NAMES))

# The broker's identity install with its exact path, content and command
# (CLOUD boat-pool.ts): Boat's file API writes the id as the sandbox user
# under /tmp, then one exec call installs it root-owned 0444. The broker accepts only exit 0 with stdout
# equal to the id.
IDENTITY_STAGING = '/tmp/ck-motor-tenant-id'
IDENTITY_INSTALL = ('sudo install -D -m 0444 -o root -g root /tmp/ck-motor-tenant-id /etc/motor/tenant/id'
                    ' && rm -f /tmp/ck-motor-tenant-id && cat /etc/motor/tenant/id')
# The broker reads this through the exec API as the sandbox user, not root,
# and counts the sandbox ready only for exit 0 and stdout equal to the id.
READY_MARKER = '/run/motor/endpoint-ready'
MARKER_READ = 'cat ' + READY_MARKER
# Upper bound, from the identity install (or from hydration when the id
# predates it), for the endpoint to listen with its marker. The measured
# value is recorded beside each check; the broker itself waits 10 minutes.
ENDPOINT_BOUND_S = 120
ENDPOINT_STATE = r'''
import json, os, pathlib, stat, subprocess
def unit(name):
    out = subprocess.run(['systemctl','show',name,'--property=ActiveState','--property=SubState',
        '--property=MainPID','--property=NRestarts','--property=Result'], capture_output=True, text=True).stdout
    return dict(line.split('=',1) for line in out.splitlines() if '=' in line)
def meta(path):
    try:
        m = os.lstat(path)
    except FileNotFoundError:
        return {'present':False}
    return {'present':True, 'uid':m.st_uid, 'mode':oct(stat.S_IMODE(m.st_mode)), 'regular':stat.S_ISREG(m.st_mode)}
rows = [line.split() for line in pathlib.Path('/proc/net/tcp').read_text().splitlines()[1:]]
etc = subprocess.run(['findmnt','-n','-o','SOURCE,FSTYPE','-T','/etc'], capture_output=True, text=True).stdout.strip()
print(json.dumps({'units':{name:unit(name) for name in ('motor-boat-restored.service','motor-boat-identity.service',
        'motor-boat.service','motor-boat-host.service')},
    'id_file':meta('/etc/motor/tenant/id'), 'marker':meta('/run/motor/endpoint-ready'),
    'listening_9443':any(r[1] == '0100007F:24E3' and r[3] == '0A' for r in rows),
    'boat_restore_active':os.path.exists('/var/lib/ascii-lazy/active'), 'etc_mount':etc}))
'''

STORAGE_STATE = r'''
import json, os, pathlib, subprocess
points = ('/var/lib/motor-boat', '/Users')
data = {'mountinfo':pathlib.Path('/proc/self/mountinfo').read_text(),
        'boot_id':pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip()}
data['findmnt'] = {point:subprocess.check_output(['findmnt','-J','-T',point,'-o','TARGET,SOURCE,FSTYPE'], text=True) for point in points}
try:
    device = pathlib.Path(points[0]).stat().st_dev
    backing = pathlib.Path(f'/sys/dev/block/{os.major(device)}:{os.minor(device)}/loop/backing_file')
    data['backing_file'] = backing.read_text().strip()
    data['expected_backing'] = pathlib.Path(data['backing_file']).samefile('/var/lib/motor-boat.xfs')
    data['users_bind'] = pathlib.Path('/Users').samefile('/var/lib/motor-boat/state/workspaces')
    data['prepared'] = json.loads(pathlib.Path('/run/motor-boat-storage.json').read_text())
except OSError as error:
    data['error'] = str(error)
data['units'] = {}
for name in ('motor-boat.service','motor-boat-host.service',r'var-lib-motor\x2dboat.mount','Users.mount'):
    text = subprocess.check_output(['systemctl','show',name,'-p','ActiveState','-p','ExecMainStartTimestampMonotonic'], text=True)
    data['units'][name] = dict(line.split('=',1) for line in text.splitlines() if '=' in line)
print(json.dumps(data))
'''


def storage_checks(data):
    filesystems = {point:json.loads(text)['filesystems'] for point, text in data['findmnt'].items()}
    actual = all(len(filesystems[point]) == 1 and filesystems[point][0]['target'] == point and filesystems[point][0]['fstype'] == 'xfs'
                 for point in ('/var/lib/motor-boat', '/Users'))
    prepared = data.get('prepared', {})
    verified = prepared.get('verified_monotonic_ns', 0)
    starts = [int(data['units'][name].get('ExecMainStartTimestampMonotonic', 0)) * 1000
              for name in ('motor-boat.service', 'motor-boat-host.service')]
    return {'reflink_disk_and_workspace_bind': actual and data.get('expected_backing') is True and data.get('users_bind') is True,
            'storage_verified_before_services': prepared.get('boot_id') == data['boot_id'] and
                prepared.get('reflink') is True and verified > 0 and all(start > verified for start in starts),
            'mount_units_active': all(data['units'][name].get('ActiveState') == 'active'
                                     for name in (r'var-lib-motor\x2dboat.mount', 'Users.mount'))}


NODE_CHECK = "node -e \"if (require('boat-dep') !== 'boat-node-modules') process.exit(1); console.log(require('boat-dep'))\""


def job_repository(repository):
    """A local tarball installs regular files in node_modules, unlike a local
    directory dependency that leaves a symlink. Publishing and reusing
    node_modules must therefore reflink those bytes."""
    (repository / 'src').mkdir()
    (repository / 'Cargo.toml').write_text('[package]\nname="boat-hello"\nversion="0.1.0"\nedition="2021"\n')
    (repository / 'Cargo.lock').write_text('version = 4\n[[package]]\nname = "boat-hello"\nversion = "0.1.0"\n')
    (repository / 'src/main.rs').write_text('fn main() { println!("boat-hello"); }\n')
    manifest = {'name':'boat-node-probe', 'version':'1.0.0', 'private':True,
                'dependencies':{'boat-dep':'file:vendor/boat-dep.tgz'}}
    (repository / 'package.json').write_text(json.dumps(manifest) + '\n')
    archive = io.BytesIO()
    with gzip.GzipFile(fileobj=archive, mode='wb', mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode='w') as tar:
            for name, content in {'package/package.json':json.dumps({'name':'boat-dep','version':'1.0.0','main':'index.js'}),
                                  'package/index.js':"module.exports = 'boat-node-modules';\n"}.items():
                info = tarfile.TarInfo(name)
                payload = content.encode()
                info.size, info.mode = len(payload), 0o644
                tar.addfile(info, io.BytesIO(payload))
    payload = archive.getvalue()
    (repository / 'vendor').mkdir()
    (repository / 'vendor/boat-dep.tgz').write_bytes(payload)
    integrity = 'sha512-' + base64.b64encode(hashlib.sha512(payload).digest()).decode()
    lock = {'name':manifest['name'], 'version':'1.0.0', 'lockfileVersion':3, 'requires':True,
            'packages':{'':manifest, 'node_modules/boat-dep':{'version':'1.0.0',
                        'resolved':'file:vendor/boat-dep.tgz', 'integrity':integrity}}}
    (repository / 'package-lock.json').write_text(json.dumps(lock) + '\n')
    for command in (['git','init','-q'], ['git','add','.'],
                    ['git','-c','user.name=Boat probe','-c','user.email=probe@invalid','commit','-qm','probe input']):
        subprocess.run(command, cwd=repository, check=True, stdout=sys.stderr)


def marker_matches(read, sid):
    return read.get('exit_code') == 0 and read.get('stdout') in (sid, sid + '\n')


def readiness_checks(wait, sid, bound=ENDPOINT_BOUND_S):
    state = wait.get('state') or {}
    marker, identity = state.get('marker', {}), state.get('id_file', {})
    return {
        'endpoint_ready_within_bound': wait.get('ready') is True and wait.get('seconds', bound + 1) <= bound,
        'ready_marker_is_exact_id': marker_matches(wait.get('marker', {}), sid),
        'ready_marker_root_0644': marker.get('uid') == 0 and marker.get('mode') == '0o644' and marker.get('regular') is True,
        'endpoint_listening': state.get('listening_9443') is True,
        'identity_root_0444': identity.get('uid') == 0 and identity.get('mode') == '0o444' and identity.get('regular') is True,
    }


def armed_checks(state):
    """The watcher is polling, and nothing of the endpoint remains: no id,
    no marker (removed on stop), nothing listening on 9443."""
    return (state['units']['motor-boat-identity.service'].get('ActiveState') == 'active' and
            state['units']['motor-boat-restored.service'].get('ActiveState') == 'active' and
            not state['id_file']['present'] and not state['marker']['present'] and not state['listening_9443'])


def job_result(text):
    result = json.loads(text)
    if type(result.get("code")) is not int or not isinstance(result.get("terminal"), dict):
        raise ValueError("client did not produce a terminal")
    return result


def isolation_result_checks(result, host):
    if result['code'] != 0:
        return {'isolation_job_completed': False}
    try:
        payload = json.loads(result['stdout'])
    except ValueError as error:
        raise ValueError('isolation job exited zero but did not emit JSON evidence') from error
    return isolation_checks(payload, host)


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
        "no_provider_job_access": provider_access_denied(job['provider_access']),
        "no_privilege_gain": job['status_security']['NoNewPrivs'] == '1' and int(job['status_security']['CapEff'],16) == 0 and int(job['status_security']['CapBnd'],16) == 0,
        **process_visibility_checks(job),
        # This explicit Boat profile does not promise a private PID namespace on systemd 255.
        "boat_pid_profile": job["namespaces"]["pid"] == host["namespaces"]["pid"],
    }


def process_visibility_checks(job):
    visibility = job['process_visibility']
    proc = visibility['proc']
    ps = visibility['ps']
    protected = set(visibility['protected_pids'])
    signal = visibility['runner_signal_probe']
    return {
        'proc_only_job_processes': bool(proc) and any(row['pid'] == visibility['self_pid'] for row in proc) and
            all(row.get('uid') == job['uid'] and row.get('same_cgroup') and row['pid'] not in protected for row in proc),
        'ps_only_job_processes': ps['exit_code'] == 0 and bool(ps['rows']) and
            any(row['pid'] == visibility['self_pid'] for row in ps['rows']) and
            all(row['uid'] == job['uid'] and row['pid'] not in protected for row in ps['rows']),
        'cannot_signal_runner': signal['pid'] in protected and signal.get('allowed') is False and signal.get('errno') == errno.EPERM,
        'pid_only_proc_mount': any('subset=pid' in line and ('hidepid=invisible' in line or 'hidepid=2' in line) for line in job['proc_mountinfo']),
    }


def provider_access_denied(access):
    return bool(access['files']) and bool(access['processes']) and not access['inherited_names'] and all(not row.get('ascii_token_assignment') and
        (not path.endswith('/.config/ascii/boat/config.json') or not row['readable'])
        for path,row in access['files'].items()) and all(not row.get('ascii_token_present') for row in access['processes'].values())


def prepared_image_digest(revision, inventory):
    inputs = {'revision':revision, 'packages':inventory['packages'], 'apt_release_files':inventory['apt_release_files']}
    return hashlib.sha256(json.dumps(inputs, sort_keys=True, separators=(',',':')).encode()).hexdigest()


def snapshot_metadata(snapshot):
    if snapshot.get('status') != 'ready' or not isinstance(snapshot.get('snapshotId'), str) or type(snapshot.get('sizeBytes')) is not int:
        raise ValueError('ready named snapshot lacks documented artifact id/size')
    return {'name':snapshot['name'], 'id':snapshot['snapshotId'], 'size_bytes':snapshot['sizeBytes'], 'state':snapshot}


SNAPSHOT_HYGIENE = r'''
import json, pathlib, pwd, sys

def image_hygiene(homes, blob, identity):
    paths = sorted({pathlib.Path(home)/'.ssh/authorized_keys' for home in homes})
    checked = []
    for path in paths:
        if path.exists():
            lines = path.read_text().splitlines()
            kept = [line for line in lines if blob not in line.split()]
            if kept != lines:
                path.write_text('\n'.join(kept)+'\n')
            if any(blob in line.split() for line in path.read_text().splitlines()):
                raise RuntimeError('probe authorization still present')
            checked.append(str(path))
    identity.unlink(missing_ok=True)
    if identity.exists() or identity.is_symlink():
        raise RuntimeError('sandbox identity still present')
    configs = [pathlib.Path(home)/'.config/ascii/boat/config.json' for home in homes]
    if any(path.exists() or path.is_symlink() for path in configs):
        raise RuntimeError('Boat CLI config is present before snapshot')
    return {'identity_absent':True,'boat_cli_configs_absent':True,'probe_authorizations_absent':True,
        'authorized_key_files_checked':checked,'private_key_uploads':0}

if __name__ == '__main__':
    homes = [account.pw_dir for account in pwd.getpwall() if account.pw_dir.startswith('/')]
    print(json.dumps(image_hygiene(homes, sys.argv[1].split()[1], pathlib.Path('/etc/motor/tenant/id'))))
'''


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

    def quiesce_image(self, sid, public, label):
        audit = json.loads(self.execute(sid, 'sudo -n -E python3 -c ' + shlex.quote(SOURCE + '\nimport json; print(json.dumps(credential_audit()))'))['stdout'])
        self.check(label + '.no_account_credentials', audit['clean'], audit)
        if not audit['clean']:
            raise ValueError('refusing to snapshot credentials or incomplete evidence')
        self.execute(sid, 'sudo -n systemctl stop motor-boat-identity motor-boat-host motor-boat-bridge motor-boat')
        hygiene = json.loads(self.execute(sid, 'sudo -n python3 -c ' + shlex.quote(SNAPSHOT_HYGIENE) + ' ' + shlex.quote(public))['stdout'])
        self.check(label + '.identity_and_probe_keys_absent', all(hygiene[k] is True for k in ('identity_absent','boat_cli_configs_absent','probe_authorizations_absent')), hygiene)
        self.execute(sid, "sudo -n sync && sudo -n systemctl stop Users.mount 'var-lib-motor\\x2dboat.mount' motor-boat-storage && "
                          'sudo -n sync && '
                          'sudo -n python3 /usr/local/libexec/motor-boat-start.py --check-mountpoints')
        return hygiene

    def save_snapshot(self, sid, name, retained=False):
        resource = {'kind':'named-snapshot','id':name}
        self.probe.data['resources'].append(resource)
        self.probe.checkpoint()
        start = time.monotonic()
        self.probe.request('POST', '/named-snapshots', {'sandboxId':sid,'name':name})
        while True:
            state = self.probe.request('GET', '/named-snapshots/' + name)['snapshot']
            if state['status'] == 'ready':
                break
            if state['status'] == 'failed':
                raise ValueError('named snapshot failed')
            time.sleep(2)
        metadata = snapshot_metadata(state)
        if retained:
            resource.update(operator_retained=True, snapshot_id=metadata['id'], size_bytes=metadata['size_bytes'])
        self.probe.checkpoint()
        return dict(metadata, seconds=time.monotonic()-start, resource=resource)

    def retain_prepared(self, sid, public, revision, inventory):
        digest = prepared_image_digest(revision, inventory)
        name = 'motor-boat-prepared-' + digest[:12]
        try:
            existing = self.probe.request('GET', '/named-snapshots/' + name)['snapshot']
        except Exception as error:
            if getattr(error, 'status', None) != 404:
                raise
            existing = None
        if existing and existing['status'] == 'ready':
            metadata = snapshot_metadata(existing)
            resource = {'kind':'named-snapshot','id':name,'operator_retained':True,'snapshot_id':metadata['id'],'size_bytes':metadata['size_bytes']}
            self.probe.data['resources'].append(resource)
            metadata['reused_existing'] = True
        else:
            hygiene = self.quiesce_image(sid, public, 'prepared_image')
            metadata = self.save_snapshot(sid, name, retained=True)
            metadata.update(hygiene=hygiene, state_unmounted=True)
        invocation = ['python3.14', str(ROOT/'script/boat-probe/probe.py'), '--key-stdin', '--cleanup', str(self.probe.results_path), '--delete-retained-images']
        metadata.update(recipe_digest=digest, operator_retained=True, real_jobs_qualified=False,
                        delete_command=shlex.join(invocation), credential_input='stdin: pipe the same authorized key reader used for this run')
        self.data['prepared_image'] = metadata
        self.probe.checkpoint()

    def install_identity(self, sid):
        """Replay the broker's identity install exactly; never raises, so a
        refused install is recorded evidence rather than an aborted phase."""
        record = {'started_monotonic': time.monotonic()}
        try:
            self.probe.request('PUT', f'/sandboxes/{sid}/files', {'path': IDENTITY_STAGING, 'content': sid})
            result = self.probe.execute(sid, IDENTITY_INSTALL)
            record.update(exit_code=result.get('exitCode'), stdout=result.get('stdout'), stderr=result.get('stderr'))
        except Exception as error:
            record['error'] = str(error)
        record['accepted'] = record.get('exit_code') == 0 and record.get('stdout') == sid
        record['seconds'] = time.monotonic() - record['started_monotonic']
        self.probe.checkpoint()
        return record

    def endpoint_state(self, sid):
        return json.loads(self.execute(sid, 'sudo -n python3 -c ' + shlex.quote(ENDPOINT_STATE))['stdout'])

    def read_marker(self, sid):
        result = self.probe.execute(sid, MARKER_READ)
        return {'exit_code': result.get('exitCode'), 'stdout': result.get('stdout'), 'stderr': result.get('stderr')}

    def wait_endpoint(self, sid, since, bound=ENDPOINT_BOUND_S):
        """Poll the marker the way the broker does until it holds the id or
        `bound` seconds have passed since `since` (a monotonic time)."""
        polls = 0
        while True:
            polls += 1
            read = self.read_marker(sid)
            ready = marker_matches(read, sid)
            if ready or time.monotonic() - since >= bound:
                return {'ready': ready, 'seconds': time.monotonic() - since, 'polls': polls, 'at_monotonic': time.monotonic(),
                        'marker': read, 'state': self.endpoint_state(sid)}
            time.sleep(2)

    def readiness(self, label, wait, sid):
        for key, passed in readiness_checks(wait, sid).items():
            self.check(label + '.' + key, passed, wait)

    def arm_then_install(self, sid, label, record):
        """Order 1: the identity watcher is already polling when the broker's
        install creates the file."""
        self.execute(sid, 'sudo -n systemctl stop motor-boat-host && sudo -n rm -f /etc/motor/tenant/id && '
                          'sudo -n systemctl restart motor-boat-identity && sudo -n systemctl start motor-boat')
        before = self.endpoint_state(sid)
        self.check(label + '.armed_before_identity', armed_checks(before), before)
        install = self.install_identity(sid)
        self.check(label + '.identity_install_accepted', install['accepted'], install)
        wait = self.wait_endpoint(sid, install['started_monotonic'])
        record['armed_then_install'] = {'before': before, 'install': install, 'wait': wait}
        self.readiness(label + '.armed', wait, sid)

    def broker_order(self, sid, label, record, pre_swap_install, hydrated_at, created_at):
        """Boat's order on a fresh clone: the broker's install may land before
        Boat's file restore finishes, and it installs again when the marker
        does not appear. Either way the endpoint must become ready."""
        first = self.wait_endpoint(sid, hydrated_at)
        flow = {'pre_swap_install': pre_swap_install, 'after_hydration': first,
                'pre_swap_install_survived': first['ready']}
        if not first['ready']:
            flow['reinstall'] = self.install_identity(sid)
            self.check(label + '.identity_reinstall_accepted', flow['reinstall']['accepted'], flow['reinstall'])
            first = flow['after_reinstall'] = self.wait_endpoint(sid, flow['reinstall']['started_monotonic'])
        record['broker_order'] = flow
        if first['ready'] and created_at is not None:
            self.data['snapshot_create_to_endpoint_ready_seconds'] = first['at_monotonic'] - created_at
        self.readiness(label + '.broker_order', first, sid)

    def existing_then_reinstall(self, sid, label, record, hydrated_at):
        """Order 2: after a resume the identity file is already on disk when
        the watcher starts. The broker then installs it again; that must keep
        the same endpoint process and marker."""
        first = self.wait_endpoint(sid, hydrated_at)
        self.readiness(label + '.existing_identity', first, sid)
        install = self.install_identity(sid)
        self.check(label + '.identity_reinstall_accepted', install['accepted'], install)
        after = self.wait_endpoint(sid, install['started_monotonic'])
        host_before = first['state']['units']['motor-boat-host.service']
        host_after = after['state']['units']['motor-boat-host.service']
        self.check(label + '.reinstall_keeps_one_endpoint', marker_matches(after['marker'], sid) and
                   host_before.get('MainPID') == host_after.get('MainPID') != '0' and
                   host_before.get('NRestarts') == host_after.get('NRestarts'), {'before': first, 'after': after})
        record['existing_then_reinstall'] = {'first': first, 'install': install, 'after': after}

    def phase(self, sid, label, directory, public, repository, client, first_started=None,
              pre_swap_install=None, hydrated_at=None):
        record = {"name": label}
        self.data["phases"].append(record)
        argv = self.route(sid, directory, public)
        if label == 'snapshot_clone':
            self.broker_order(sid, label, record, pre_swap_install, hydrated_at, first_started)
        elif label == 'prepared_clone':
            install = self.install_identity(sid)
            record['identity_install'] = install
            self.check(label + '.identity_install_accepted', install['accepted'], install)
            self.readiness(label, self.wait_endpoint(sid, install['started_monotonic']), sid)
        elif label == 'resumed':
            self.existing_then_reinstall(sid, label, record, hydrated_at)
        else:
            self.arm_then_install(sid, label, record)
        record['storage'] = json.loads(self.execute(sid, 'sudo -n python3 -c ' + shlex.quote(STORAGE_STATE))['stdout'])
        for key, passed in storage_checks(record['storage']).items():
            self.check(label + '.' + key, passed, record['storage'])
        self.execute(sid, 'sudo -n systemctl start motor-boat-bridge')
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
        self.check(label + '.no_provider_host_environment', not host['idle_services_memory']['motor-boat-host']['provider_environment_names'], host['idle_services_memory']['motor-boat-host'])
        self.check(label + '.outside_job_cannot_read_provider_tokens', provider_access_denied(host['provider_sanitization']['outside_job_after']), host['provider_sanitization'])
        self.check(label + '.protected_services_running', bool(host['sshd_pids']) and host['runner_pid'] > 0 and host['idle_services_memory']['motor-boat-host']['main_pid'] > 0, {'runner':host['runner_pid'],'host':host['idle_services_memory']['motor-boat-host']['main_pid'],'sshd':host['sshd_pids']})
        protected = [host['runner_pid'], host['idle_services_memory']['motor-boat-host']['main_pid'], *host['sshd_pids'], *[p['pid'] for p in host['provider_sanitization']['processes']]]
        job_script = 'PROVIDER_PATHS=' + repr(host['provider_sanitization']['paths_checked']) + '\nPROVIDER_PIDS=' + repr([p['pid'] for p in host['provider_sanitization']['processes']]) + '\nRUNNER_PID=' + repr(host['runner_pid']) + '\nPROTECTED_PIDS=' + repr(protected) + '\n' + JOB
        with tunnel(argv, directory, self.probe.deadline, record.setdefault("tunnel", {})) as address:
            with tempfile.TemporaryDirectory(prefix="motor-boat-client-") as storage:
                for name, command in [("node_modules_first", NODE_CHECK), ("node_modules_reuse", NODE_CHECK),
                                      ("exit", "printf 'boat-round-trip\\n'; exit 23"),
                                      ("rust", 'cargo +1.99.0 build --offline --locked && "$CARGO_TARGET_DIR/debug/boat-hello"'),
                                      ("isolation", "python3 -c " + shlex.quote(job_script))]:
                    result = process([str(client), address, str(repository), storage, command], timeout=min(150, max(1, int(self.probe.deadline-time.monotonic()))))
                    record[name] = result
                    if result.get("exit_code") != 0:
                        raise ValueError("motor client failed: " + json.dumps(result))
                    parsed = job_result(result["stdout"])
                    record[name]["result"] = parsed
                    if first_started is not None and name == 'node_modules_first' and parsed['code'] == 0 and 'boat-node-modules' in parsed['stdout']:
                        self.data["snapshot_create_to_first_job_seconds"] = time.monotonic() - first_started
                    if name == "isolation":
                        for key, passed in isolation_result_checks(parsed, host).items():
                            self.check(label + "." + key, parsed["code"] == 0 and passed, parsed)
                    else:
                        expected = 23 if name == "exit" else 0
                        marker = "boat-round-trip" if name == "exit" else ("boat-node-modules" if name.startswith('node_modules_') else "boat-hello")
                        self.check(label + "." + name, parsed["code"] == expected and marker in parsed["stdout"], parsed)
        self.check(label + ".tunnel_reaped", record["tunnel"].get("reaped") is True, record["tunnel"])
        if any(check['status'] != 'PASS' for check in self.data['checks'] if check['name'].startswith(label + '.')):
            raise ValueError('real job or host checks failed in phase ' + label + '; no snapshot qualification claimed')
        return record

    def run(self):
        self.directory = None
        try:
            self._run()
        finally:
            self.data["local_private_keys_removed"] = self.directory is None or not self.directory.exists()
            self.probe.checkpoint()

    def verify_prepared(self, name, directory, public, client):
        self.data['prepared_source'] = snapshot_metadata(self.probe.request('GET', '/named-snapshots/' + name)['snapshot'])
        self.data['prepared_source']['borrowed_not_deleted'] = True
        started = time.monotonic()
        item, self.data['create_from_snapshot'] = self.probe.create(name)
        sid = item['id']
        self.probe.wait_hydrated(sid)
        self.data['create_from_snapshot']['seconds_to_hydrated'] = time.monotonic() - started
        with tempfile.TemporaryDirectory(prefix='boat-job-', dir=ROOT / '.cortexkit') as repo:
            repository = Path(repo)
            job_repository(repository)
            # Do not restart, reconfigure or mount anything in the guest: that
            # would repair the startup order this mode is meant to observe.
            self.phase(sid, 'prepared_clone', directory, public, repository, client, first_started=started)
            self.probe.request('POST', f'/sandboxes/{sid}/stop', {})
            self.probe.wait_sandbox(sid, {'archived'})
            self.probe.request('POST', f'/sandboxes/{sid}/resume', {})
            from probe import READY
            self.probe.wait_sandbox(sid, READY)
            self.probe.wait_hydrated(sid)
            self.phase(sid, 'resumed', directory, public, repository, client, hydrated_at=time.monotonic())
        self.data['all_passed'] = bool(self.data['checks']) and all(c['status'] == 'PASS' for c in self.data['checks'])
        self.probe.checkpoint()

    def _run(self):
        from probe import READY, cleanup
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=ROOT, check=True, stdout=sys.stderr)
        build = process(["env", "-u", "GIT_CONFIG_COUNT", "-u", "GIT_CONFIG_KEY_0", "-u", "GIT_CONFIG_VALUE_0",
                         "cargo", "build", "--locked", "-p", "motor-module", "--example", "boat_probe"], timeout=600)
        self.data["client_build"] = build
        if build.get("exit_code") != 0:
            raise ValueError("local operator client build failed")
        client = ROOT / "target/debug/examples/boat_probe"
        # The 30-minute provider budget starts after the local client is available.
        self.probe.deadline = time.monotonic() + min(1800, self.probe.minutes * 60)
        self.data["source_revision"] = revision
        (ROOT / '.cortexkit').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="motor-boat-ssh-") as temporary:
            directory = Path(temporary)
            self.directory = directory
            generated = process(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(directory / "key")])
            if generated.get("exit_code") != 0:
                raise ValueError("temporary SSH key generation failed")
            public = (directory / "key.pub").read_text().strip()
            verify_image = getattr(self.probe, 'verify_prepared_image', None)
            if verify_image:
                self.verify_prepared(verify_image, directory, public, client)
                return
            source_image = getattr(self.probe, 'from_prepared_image', None)
            if source_image:
                self.data['prepared_source'] = snapshot_metadata(self.probe.request('GET', '/named-snapshots/' + source_image)['snapshot'])
                self.data['prepared_source']['borrowed_not_deleted'] = True
            item, self.data["create"] = self.probe.create(source_image)
            sid = item["id"]
            if item.get("vcpu") != 8 or item.get("memoryGB") != 16:
                raise ValueError("unexpected large sandbox sizing")
            if source_image:
                self.probe.wait_hydrated(sid)
                # A prepared image from an older recipe still has the path unit.
                self.execute(sid, 'sudo -n systemctl stop motor-boat-host motor-boat-bridge motor-boat && '
                                  '{ sudo -n systemctl stop motor-boat-host.path motor-boat-identity 2>/dev/null || true; }')
            argv = self.route(sid, directory, public)
            bundle = directory / "motor.bundle"
            subprocess.run(["git", "bundle", "create", str(bundle), "HEAD"], cwd=ROOT, check=True, stdout=sys.stderr)
            sha = hashlib.sha256(bundle.read_bytes()).hexdigest()
            with bundle.open("rb") as stream:
                sent = subprocess.run(argv + ["cat > /home/user/motor.bundle"], stdin=stream, capture_output=True, timeout=120)
            if sent.returncode:
                raise ValueError("source upload over pinned SSH failed: " + sent.stderr.decode(errors="replace"))
            install = (f"cd /home/user && echo '{sha}  motor.bundle' | sha256sum -c - && "
                       "sudo -n mkdir -p /srv/build && if sudo -n test -d /srv/build/source/.git; then "
                       "sudo -n git -C /srv/build/source fetch /home/user/motor.bundle HEAD; else "
                       "sudo -n git clone motor.bundle /srv/build/source; fi && "
                       f"sudo -n git -C /srv/build/source checkout --detach {revision}")
            started = time.monotonic()
            self.data["source_upload"] = self.execute(sid, install)
            self.data["apt_preflight"] = json.loads(self.execute(sid, "sudo -n python3 /srv/build/source/script/boat-image/install.py --apt-preflight-only", timeout=180)["stdout"])
            self.probe.checkpoint()
            self.data["install"] = self.execute(sid, f"sudo -n python3 /srv/build/source/script/boat-image/install.py --revision {revision}", timeout=max(1, int(self.probe.deadline-time.monotonic())-120))
            self.data["install_seconds"] = time.monotonic() - started
            if getattr(self.probe, 'retain_prepared_image', False):
                inventory = json.loads(self.execute(sid, 'sudo -n cat /var/lib/motor-boat/inventory.json')['stdout'])
                self.retain_prepared(sid, public, revision, inventory)
            with tempfile.TemporaryDirectory(prefix="boat-job-", dir=ROOT / ".cortexkit") as repo:
                repository = Path(repo)
                job_repository(repository)
                self.phase(sid, "initial", directory, public, repository, client)
                hygiene = self.quiesce_image(sid, public, 'snapshot')
                name = "motor-runner-" + self.probe.data["run_id"]
                self.data['snapshot'] = dict(self.save_snapshot(sid, name), hygiene=hygiene, state_unmounted=True)
                original = next(r for r in self.probe.data["resources"] if r["id"] == sid)
                subset = {"resources": [original], "pending_creates": [], "cleanup": self.probe.data["cleanup"]}
                cleanup(self.probe.client, subset, self.probe.checkpoint)
                state = original.get("deletion_state", {})
                if not original.get("deleted") and state.get("stage") != "waiting_for_uploads":
                    raise ValueError("original is not confirmed stopped/deletion-accepted; refusing a concurrent clone")
                start = time.monotonic()
                clone, self.data["create_from_snapshot"] = self.probe.create(name)
                sid = clone["id"]
                # Install while Boat may still be restoring files, as a broker
                # racing the restore would; broker_order re-installs if lost.
                pre_swap = self.install_identity(sid)
                self.probe.wait_hydrated(sid)
                hydrated_at = time.monotonic()
                self.data['create_from_snapshot']['seconds_to_hydrated'] = hydrated_at - start
                self.phase(sid, "snapshot_clone", directory, public, repository, client, first_started=start,
                           pre_swap_install=pre_swap, hydrated_at=hydrated_at)
                self.probe.request("POST", f"/sandboxes/{sid}/stop", {})
                self.probe.wait_sandbox(sid, {"archived"})
                self.probe.request("POST", f"/sandboxes/{sid}/resume", {})
                self.probe.wait_sandbox(sid, READY)
                self.probe.wait_hydrated(sid)
                self.phase(sid, "resumed", directory, public, repository, client, hydrated_at=time.monotonic())
                before = self.data["phases"][1]["host"]["ssh_config"]
                after = self.data["phases"][2]["host"]["ssh_config"]
                self.check("sshd.dropin_survives_resume", before.get("/etc/ssh/sshd_config.d/00-motor-boat.conf") == after.get("/etc/ssh/sshd_config.d/00-motor-boat.conf") and bool(after.get("/etc/ssh/sshd_config.d/00-motor-boat.conf")), {"before": before, "after": after})
        self.data["local_private_keys_removed"] = not directory.exists()
        self.data["all_passed"] = bool(self.data["checks"]) and all(c["status"] == "PASS" for c in self.data["checks"])
        if self.data.get('prepared_image'):
            self.data['prepared_image']['real_jobs_qualified'] = self.data['all_passed']
        self.probe.checkpoint()
