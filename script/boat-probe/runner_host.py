"""Real motor jobs in a disposable Boat host. The admission endpoint is started
the way the production broker does it (identity install, then polling the
readiness marker) in each order the broker can produce. The endpoint only
trusts the production broker's signing keys, so no token it accepts is used."""
import contextlib
import base64
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import subprocess
from subprocess import Popen as _quota_popen
import sys
import tarfile
import tempfile
import time
from typing import Any

from credential_audit import SOURCE
from pid_namespace import PID_SOURCE, TEARDOWN_SOURCE, bootstrap_checks, control_identity_command, lifecycle_failure, outside_control_passes, pid_checks, pid_failure_reason, reference_command, teardown_checks, teardown_job
from probe import provider_sizing_matches, sizing_error
from ssh_reach import endpoint, pin_host, process, ssh_argv

ROOT = Path(__file__).resolve().parents[2]
provider_spec = importlib.util.spec_from_file_location('boat_provider_access', ROOT / 'script/boat-image/motor-boat-credentials.py')
if provider_spec is None or provider_spec.loader is None:
    raise ImportError('cannot load provider access probe')
provider = importlib.util.module_from_spec(provider_spec)
provider_spec.loader.exec_module(provider)
PORT = 19443
PARITY_SCRIPT = ROOT / "script/runner/toolchain-parity.py"
PARITY_MANIFEST = ROOT / "script/runner/toolchain.json"
CARGO_SEED_SCRIPT = ROOT / 'script/runner/cargo-seed.py'
SYSCALL_SURFACE_SCRIPT = ROOT / 'script/runner/syscall-surface.py'
CARGO_OFFLINE_PROBE = ROOT / 'script/runner/cargo-offline-probe.py'
GIT_PUSHURL_PROBE = ROOT / 'script/runner/git-pushurl-probe.py'
WASM_OPT_PROBE = ROOT / 'script/runner/wasm-opt-probe.py'
ROOT_PATHS_SCRIPT = ROOT / 'script/tenant-image/guest/root_paths.py'


def upload_seed_input(argv, source, name):
    with Path(source).open('rb') as stream:
        sent = subprocess.run(argv + ['umask 077; cat > /home/user/' + name],
                              stdin=stream, capture_output=True, timeout=300)
    if sent.returncode:
        raise ValueError('Cargo seed upload over pinned SSH failed: ' + sent.stderr.decode(errors='replace'))


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

EXEC_PROBE = r'''
import os, pathlib, shutil, subprocess, tempfile

def execution_capabilities():
    def command(argv):
        try:
            result = subprocess.run(argv, capture_output=True, text=True, timeout=10)
            return {'exit_code':result.returncode, 'stdout':result.stdout, 'stderr':result.stderr}
        except (OSError, subprocess.TimeoutExpired) as error:
            return {'error':str(error), 'errno':getattr(error, 'errno', None)}

    def succeeded(result, output):
        return result.get('exit_code') == 0 and result.get('stdout', '').strip() == output

    tmp = os.environ.get('TMPDIR')
    bun = shutil.which('bun')
    if bun:
        bun = os.path.abspath(bun)
    mounts = {}
    for name, path in [('TMPDIR', tmp), ('/tmp', '/tmp'), ('bun', bun)]:
        mounts[name] = {'path':path, 'resolved':os.path.realpath(path) if path else None,
                        'mount':command(['findmnt', '--json', '--target', path, '--output', 'TARGET,SOURCE,FSTYPE,OPTIONS']) if path else {'error':'path unavailable'}}
    result = {'tmpdir':tmp, 'bun':bun, 'mounts':mounts,
              'b_bin_sh':command(['/bin/sh', '-c', 'printf boat-sh-ok']),
              'a_tmpdir':{}, 'c_bun_shebang':{}}
    try:
        if not tmp:
            raise OSError('TMPDIR is unset')
        with tempfile.TemporaryDirectory(prefix='boat-exec-', dir=tmp) as directory:
            script = pathlib.Path(directory)/'shell-script'
            linked = script.with_name('shell-script-link')
            script.write_text('#!/bin/sh\nprintf boat-tmp-ok\n')
            script.chmod(0o755)
            result['a_tmpdir']['exec'] = command([str(script)])
            try:
                os.link(script, linked)
                result['a_tmpdir']['hardlink'] = {'same_inode':script.stat().st_ino == linked.stat().st_ino}
                result['a_tmpdir']['link_exec'] = command([str(linked)])
            except OSError as error:
                result['a_tmpdir']['hardlink'] = {'error':str(error), 'errno':error.errno}
            if bun:
                script = script.with_name('bun-script')
                shebang = '#!' + bun + '\n'
                script.write_text(shebang + "console.log('boat-bun-ok');\n")
                script.chmod(0o755)
                result['c_bun_shebang'] = {'shebang':shebang.strip(), 'shebang_bytes':len(shebang.encode()), 'exec':command([str(script)])}
            else:
                result['c_bun_shebang'] = {'error':'bun not found on PATH'}
    except OSError as error:
        result['a_tmpdir']['error'] = str(error)
        result['c_bun_shebang'].setdefault('error', 'cannot create shebang script in TMPDIR: ' + str(error))
    a = result['a_tmpdir']
    result['checks'] = {
        'a_tmpdir_hardlink_exec': a.get('hardlink', {}).get('same_inode') is True and all(succeeded(a.get(key, {}), 'boat-tmp-ok') for key in ('exec', 'link_exec')),
        'b_bin_sh': succeeded(result['b_bin_sh'], 'boat-sh-ok'),
        'c_bun_shebang': succeeded(result['c_bun_shebang'].get('exec', {}), 'boat-bun-ok'),
    }
    result['failures'] = [name for name, passed in result['checks'].items() if not passed]
    return result
'''

JOB = provider.ACCESS_SOURCE + EXEC_PROBE + PID_SOURCE + r'''
import errno, json, os, pathlib, pwd, subprocess
exec_inherited_fds = inherited_fds()
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
data['proc_mountinfo'] = [line for line in pathlib.Path('/proc/self/mountinfo').read_text().splitlines() if line.split()[4] in ('/proc', '/proc/sys')]
def global_proc():
    # Build-server jobs read these non-process entries (ps needs btime from
    # /proc/stat; lock-holder and socket-readiness tests read /proc/locks and
    # /proc/net/unix); /proc/sys must stay read-only and the kernel command line
    # must not carry provider credentials. Record booleans, not the contents.
    # A None prefix only requires the entry to be readable, since it may be empty.
    out = {}
    for name, path, prefix in (('stat', '/proc/stat', 'btime '), ('meminfo', '/proc/meminfo', 'MemTotal:'),
                               ('locks', '/proc/locks', None), ('net_unix', '/proc/net/unix', 'Num ')):
        try:
            lines = pathlib.Path(path).read_text().splitlines()
            out[name] = {'readable': True,
                         'has_line': prefix is None or any(line.startswith(prefix) for line in lines)}
        except OSError as error:
            out[name] = {'readable': False, 'error': type(error).__name__}
    try:
        out['sys_read_only'] = bool(os.statvfs('/proc/sys').f_flag & os.ST_RDONLY)
    except OSError as error:
        out['sys_read_only'], out['sys_error'] = False, type(error).__name__
    try:
        cmdline = pathlib.Path('/proc/cmdline').read_text()
        out['cmdline'] = {'readable': True, 'provider_names': 'ASCII_TOKEN' in cmdline or 'BOAT_' in cmdline}
    except OSError as error:
        out['cmdline'] = {'readable': False, 'error': type(error).__name__}
    return out
data['proc_global'] = global_proc()
try:
    same_target = os.path.samefile('target', '/run/motor/target')
    data['cargo_target'] = {'env': os.environ.get('CARGO_TARGET_DIR'), 'same_directory': same_target}
except OSError as error:
    data['cargo_target'] = {'env': os.environ.get('CARGO_TARGET_DIR'), 'same_directory': None, 'error': type(error).__name__}
data['execution_capabilities'] = execution_capabilities()
data['pid_namespace'] = pid_evidence(PROTECTED_IDENTITIES)
data['pid_namespace']['inherited_fds'] = exec_inherited_fds
data['process_visibility']['runner_signal_probe'] = next(row for row in data['pid_namespace']['signals'] if row['pid'] == RUNNER_PID)
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
def endpoint_config():
    # The configuration the running endpoint was started with (its --sandbox argument).
    try:
        argv = [part.decode() for part in pathlib.Path('/proc/'+str(idle['motor-boat-host']['main_pid'])+'/cmdline').read_bytes().split(b'\0') if part]
        path = argv[argv.index('--sandbox') + 1]
        return path, json.loads(pathlib.Path(path).read_text())
    except (OSError, ValueError, IndexError) as error:
        return None, {'error': str(error)}
def state_disk():
    try:
        disk = os.statvfs('/var/lib/motor-boat/state')
        return {'total_bytes': disk.f_blocks * disk.f_frsize, 'free_bytes': disk.f_bavail * disk.f_frsize}
    except OSError as error:
        return {'error': str(error)}
# The sandbox's real CPU count and the counts the runner and endpoint were started with.
endpoint_path, endpoint = endpoint_config()
sizing = {'nproc': int(subprocess.check_output(['nproc'], text=True).strip()),
          'affinity_cpus': len(os.sched_getaffinity(0)),
          'runner_argv': [part.decode() for part in pathlib.Path('/proc/'+pid+'/cmdline').read_bytes().split(b'\0') if part],
          'endpoint_config_path': endpoint_path, 'endpoint_config': endpoint, 'state_disk': state_disk()}
print(json.dumps({'namespaces': {n: os.readlink('/proc/self/ns/'+n) for n in ('net','pid','mnt')},
    'systemd_version':int(subprocess.check_output(['systemctl', '--version'], text=True).split()[1]),
    'credential_audit': credential_audit(), 'ssh_config': configs,
    'sshd': capture(['/usr/sbin/sshd','-T']),
    'systemd_env': capture(['systemd-run','--wait','--pipe','--collect','python3','-c',ENV_NAMES]),
    'runner_unit': capture(['systemctl','show','motor-boat','--property=MainPID','--property=Environment']),
    'runner_environment': {k: True for k in names if k == 'ASCII_TOKEN' or k.startswith('BOAT_')},
    'memory_budget': json.loads(pathlib.Path('/var/lib/motor-boat/budget.json').read_text()),
    'idle_services_memory': idle, 'meminfo_at_idle': pathlib.Path('/proc/meminfo').read_text(),
    'endpoint_refusal': endpoint_refusal(), 'tenant_sockets': tenant_sockets,
    'runner_pid':int(pid),
    'sizing':sizing,
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

STORAGE_TIMELINE = r'''
import json, os, pathlib, subprocess, time
samples = []
for _ in range(12):
    rows = [line for line in pathlib.Path('/proc/self/mountinfo').read_text().splitlines()
            if line.split()[4] in ('/Users', '/var/lib/motor-boat')]
    samples.append({'monotonic':time.monotonic(), 'mounts':rows,
        'ready':pathlib.Path('/run/motor/endpoint-ready').exists(),
        'devices':[os.stat(p).st_dev for p in ('/Users','/var/lib/motor-boat')]})
    time.sleep(1)
print(json.dumps(samples))
'''

IDLE_RECLAIM = r'''
import json, pathlib, subprocess, time
started = time.monotonic()
state = pathlib.Path('/run/ascii-idle-reclaim')
def read_state():
    try:
        return state.read_text()
    except FileNotFoundError:
        return None
before = read_state()
# Fresh clones may not have reached the timer's first five-minute invocation.
# Seed counters through the real service, never by fabricating its state file.
subprocess.run(['systemctl','start','ascii-idle-reclaim.timer'],check=True)
subprocess.run(['systemctl','start','ascii-idle-reclaim.service'],check=True)
seeded = read_state()
while True:
    time.sleep(30)
    after = read_state()
    fields = (after or '').split()
    elapsed = time.monotonic() - started
    reclaimed = len(fields) == 6 and int(fields[4]) >= 5 and int(fields[5]) >= int(started)
    if (elapsed >= 420 and reclaimed) or elapsed >= 1020:
        break
unit = subprocess.check_output(['systemctl','show','ascii-idle-reclaim.service',
    '-p','ExecMainStartTimestampMonotonic','-p','Result'],text=True)
print(json.dumps({'seconds':elapsed, 'started':started, 'before':before, 'seeded':seeded,
    'after':after, 'unit':unit, 'reclaimed_during_idle':reclaimed}))
'''

# Collected when the probe client gets no terminal for a real job, before the
# tunnel closes and the sandbox is cleaned up. The client cannot tell which
# stage stopped (submission, setup, the command, or finishing), so this records
# what each side was doing: the runner, endpoint and bridge journals, every
# thread's kernel stack and wait channel for the runner and endpoint, user-space
# stacks when a stack dumper is installed, the job units and their processes,
# the runner's records of recent jobs with the tails of their output files, and
# the setup phases the runner reported (a job's setup, including its JavaScript
# install, runs before the command's timeout starts, so a stuck setup never
# times out on its own). Paths are constants so tests can point them at a
# fixture tree; STATE is the runner's state directory (state root + instance).
JOB_HANG_DIAGNOSTICS = r'''
import json, os, shutil, subprocess, time
PROC = '/proc'
CGROUP = '/sys/fs/cgroup'
STATE = '/var/lib/motor-boat/state/boat'
UNITS = ('motor-boat.service', 'motor-boat-host.service', 'motor-boat-bridge.service')
NAMES = ('motor-runner', 'motor-host')
RECENT_S = 900
out = {'monotonic': time.monotonic(), 'wall': time.time()}
def command(argv, timeout=30, limit=262144):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, errors='replace', timeout=timeout)
    except (OSError, subprocess.TimeoutExpired) as error:
        return {'argv': argv, 'error': str(error)}
    return {'argv': argv, 'exit_code': result.returncode, 'stdout': result.stdout[-limit:],
            'stdout_truncated': len(result.stdout) > limit, 'stderr': result.stderr[-8192:]}
def read(path, limit=65536):
    try:
        with open(path, 'rb') as stream:
            data = stream.read(limit)
    except OSError as error:
        return {'error': str(error)}
    return data.decode(errors='replace')
def tail(path, limit=8192):
    try:
        with open(path, 'rb') as stream:
            size = os.fstat(stream.fileno()).st_size
            stream.seek(max(0, size - limit))
            return {'size': size, 'tail': stream.read(limit).decode(errors='replace')}
    except OSError as error:
        return {'error': str(error)}
def tasks(pid):
    rows = {}
    try:
        names = sorted(os.listdir(f'{PROC}/{pid}/task'), key=int)
    except OSError as error:
        return {'error': str(error)}
    for task in names:
        base = f'{PROC}/{pid}/task/{task}'
        stat = read(base + '/stat')
        rows[task] = {'comm': read(base + '/comm'), 'wchan': read(base + '/wchan'),
                      'state': stat.rsplit(')', 1)[-1].split()[0] if isinstance(stat, str) and ')' in stat else stat,
                      'syscall': read(base + '/syscall'), 'stack': read(base + '/stack')}
    return rows
def user_stacks(pid):
    # Kernel stacks of a blocked Rust thread end in futex_wait; only a
    # user-space stack names the lock it waits for.
    if shutil.which('eu-stack'):
        return command(['eu-stack', '-p', str(pid)], timeout=60)
    if shutil.which('gdb'):
        return command(['gdb', '-p', str(pid), '-batch', '-nx', '-ex', 'set pagination off',
                        '-ex', 'thread apply all bt'], timeout=90)
    return {'error': 'neither eu-stack nor gdb is installed'}
out['journal'] = command(['journalctl', '--no-pager', '-o', 'short-iso-precise', '--since', '-5min',
                          *[flag for unit in UNITS for flag in ('-u', unit)], '-u', 'motor-job-*'])
out['units'] = {unit: command(['systemctl', 'show', unit, '-p', 'MainPID', '-p', 'ActiveState',
                               '-p', 'SubState', '-p', 'NRestarts']) for unit in UNITS}
out['job_units'] = command(['systemctl', 'status', '--no-pager', '-l', '--lines=20', 'motor-job-*'])
service_pids = set()
for unit in UNITS:
    text = out['units'][unit].get('stdout', '')
    for line in text.splitlines():
        if line.startswith('MainPID=') and line[8:].isdigit() and int(line[8:]) > 0:
            service_pids.add(int(line[8:]))
for entry in os.listdir(PROC):
    if entry.isdigit() and read(f'{PROC}/{entry}/comm', 64) in [name + '\n' for name in NAMES]:
        service_pids.add(int(entry))
job_pids = set()
for root, directories, _ in os.walk(CGROUP):
    # Each job unit's cgroup, and the runner's own: a job's setup, including
    # its JavaScript install, runs as children of the runner before the job
    # unit exists. Nested cgroups count as their ancestor's.
    parts = os.path.relpath(root, CGROUP).split(os.sep)
    if any(part.startswith('motor-job-') or part == 'motor-boat.service' for part in parts):
        procs = read(root + '/cgroup.procs')
        if isinstance(procs, str):
            job_pids.update(int(pid) for pid in procs.split())
out['cgroup_pids'] = sorted(job_pids)
out['processes'] = {}
for pid in sorted(service_pids) + sorted(job_pids - service_pids)[:50]:
    cmdline = read(f'{PROC}/{pid}/cmdline', 4096)
    out['processes'][str(pid)] = {'comm': read(f'{PROC}/{pid}/comm'),
        'cmdline': cmdline.replace('\0', ' ') if isinstance(cmdline, str) else cmdline,
        'status': read(f'{PROC}/{pid}/status'), 'tasks': tasks(pid)}
for pid in sorted(service_pids):
    if read(f'{PROC}/{pid}/comm', 64) == 'motor-runner\n':
        out['processes'][str(pid)]['user_stacks'] = user_stacks(pid)
ps = command(['ps', '-eLo', 'pid,tid,ppid,user:12,stat,wchan:32,etimes,pcpu,comm,args', '--sort=pid'])
kept = set(service_pids) | job_pids
rows = ps.get('stdout', '').splitlines()
ps['stdout'] = '\n'.join(rows[:1] + [row for row in rows[1:] if 'motor' in row
                         or (row.split() and row.split()[0].isdigit() and int(row.split()[0]) in kept)])
out['ps'] = ps
out['loadavg'] = read(f'{PROC}/loadavg')
out['meminfo'] = read(f'{PROC}/meminfo')
out['pressure'] = {name: read(f'{PROC}/pressure/{name}') for name in ('cpu', 'memory', 'io')}
out['df'] = command(['df', '-B1', '/var/lib/motor-boat', '/'])
now = time.time()
out['recent_jobs'] = {}
jobs = os.path.join(STATE, 'jobs')
for name in sorted(os.listdir(jobs)) if os.path.isdir(jobs) else []:
    record = os.path.join(jobs, name, 'record.json')
    try:
        if now - os.stat(record).st_mtime > RECENT_S:
            continue
        with open(record) as stream:
            job = json.load(stream)
    except (OSError, ValueError) as error:
        out['recent_jobs'][name] = {'error': str(error)}
        continue
    summary = {key: job.get(key) for key in ('state', 'accepted_ms', 'queued_ms', 'setup_ms', 'started_ms',
               'finished_ms', 'execution_ended_ms', 'process', 'kill', 'terminal', 'next_seq', 'output_bytes',
               'weight', 'base', 'durable_output', 'stdout_offset', 'stderr_offset', 'monitoring_error')}
    summary['command'] = (job.get('request') or {}).get('command')
    output = os.path.join(STATE, 'sandboxes', name, 'output')
    summary['output'] = {stream: tail(os.path.join(output, stream)) for stream in ('stdout', 'stderr', 'exit')}
    out['recent_jobs'][name] = summary
out['tenant_events'] = tail(os.path.join(STATE, 'tenant-events.json'), 65536)
print(json.dumps(out))
'''

# Run only in a disposable probe clone, after its real-job and resume phases.
# Reproduce Boat removing /Users after readiness, then directly start Users.mount
# as a job dependency could, over a newly created root-disk workspace directory.
# Allow three nominal watcher polls, observe late withdrawal for up to ten
# seconds, then watch ten more seconds for an endpoint republishing readiness.
STORAGE_WITHDRAWAL_MAX_S = 3
DEFERRED_CLEANUP_REPLAY = r'''
import json, pathlib, subprocess, time, uuid
def capture(command):
    try:
        result = subprocess.run(command,capture_output=True,text=True,timeout=10)
        return {'returncode':result.returncode, 'stdout':result.stdout, 'stderr':result.stderr}
    except subprocess.TimeoutExpired:
        return {'returncode':None, 'stdout':'', 'stderr':'timed out after 10 seconds'}
def watch_state():
    result = capture(['systemctl','show','motor-boat-storage-watch.service',
        '-p','ActiveState','-p','SubState','-p','NRestarts','-p','ExecMainStartTimestamp'])
    result['properties'] = dict(line.split('=',1) for line in result['stdout'].splitlines() if '=' in line)
    return result
watch_before = watch_state()
ready = pathlib.Path('/run/motor/endpoint-ready')
ready_before = ready.exists()
started = time.monotonic()
subprocess.run(['umount','--','/Users'],check=True,timeout=10)
sentinel = pathlib.Path('/Users') / ('motor-mount-replay-' + uuid.uuid4().hex)
sentinel.mkdir()
(sentinel / 'materialized-workspace').touch()
# Do not let a slow mount-unit activation hide the time of readiness withdrawal.
activation = subprocess.Popen(['systemctl','start','Users.mount'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
latency = 'never'
while True:
    elapsed = time.monotonic() - started
    if not ready.exists():
        latency = elapsed
        break
    if elapsed >= 10:
        break
    time.sleep(min(.1, 10 - elapsed))
reappeared = False
reappearance_latency = 'never'
observed_after_withdrawal = 0
if latency != 'never':
    withdrawn_at = time.monotonic()
    while True:
        observed_after_withdrawal = time.monotonic() - withdrawn_at
        if ready.exists() and not reappeared:
            reappeared = True
            reappearance_latency = observed_after_withdrawal
        if observed_after_withdrawal >= 10:
            break
        time.sleep(min(.1, 10 - observed_after_withdrawal))
activation_timed_out = False
try:
    _, activation_stderr = activation.communicate(timeout=10)
except subprocess.TimeoutExpired:
    activation_timed_out = True
    activation.kill()
    _, activation_stderr = activation.communicate()
rows = [line for line in pathlib.Path('/proc/self/mountinfo').read_text().splitlines() if line.split()[4] == '/Users']
endpoint_ready = ready.exists()
watch_after = watch_state()
watch_journal = capture(['journalctl','--boot=0','--unit=motor-boat-storage-watch.service','--no-pager','--output=short-monotonic'])
print(json.dumps({'activation_code':activation.returncode, 'activation_stderr':activation_stderr,
    'activation_timed_out':activation_timed_out,
    'workspace_visible':(sentinel / 'materialized-workspace').exists(), 'users_mounts':rows,
    'endpoint_ready_before':ready_before, 'endpoint_ready':endpoint_ready,
    'withdrawal_latency_seconds':latency,
    'endpoint_ready_reappeared':reappeared, 'reappearance_latency_seconds':reappearance_latency,
    'post_withdrawal_observation_seconds':observed_after_withdrawal,
    'storage_watch_before':watch_before, 'storage_watch_after':watch_after, 'storage_watch_journal':watch_journal}))
'''


def cleanup_replay_checks(data):
    latency = data.get('withdrawal_latency_seconds')
    return (data['workspace_visible'] and not data['users_mounts'] and not data['endpoint_ready']
            and data.get('endpoint_ready_before') is True
            and type(latency) in (int, float) and 0 <= latency <= STORAGE_WITHDRAWAL_MAX_S
            and data.get('endpoint_ready_reappeared') is False
            and data.get('post_withdrawal_observation_seconds', 0) >= 10)


def idle_reclaim_checks(data):
    fields = (data.get('after') or '').split()
    return (data['seconds'] >= 420 and len(fields) == 6 and int(fields[4]) >= 5 and
            int(fields[5]) >= int(data['started']) and data.get('reclaimed_during_idle') is True and
            'Result=success' in data['unit'])


def idle_reclaim_status(data):
    # Preserve the provider's raw counters rather than infer an undocumented
    # reclaim event from any one field changing after a successful service run.
    try:
        if idle_reclaim_checks(data):
            return 'PASS'
        fields = [int(value) for value in (data.get('after') or '').split()]
        if (data['seconds'] >= 420 and len(fields) == 6 and 'Result=success' in data['unit']
                and data.get('reclaimed_during_idle') is False):
            return 'INCONCLUSIVE'
    except (KeyError, ValueError, TypeError):
        pass
    return 'FAIL'


def storage_timeline_checks(samples):
    return bool(samples) and all(
        sample['ready'] and len(sample['mounts']) == 2 and sample['mounts'] == samples[0]['mounts'] and
        sample['devices'][0] == sample['devices'][1] and
        all(' - xfs ' in row for row in sample['mounts'])
        for sample in samples)


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


# Committed offline fixtures locked with bun.lock and pnpm-lock.yaml; the root
# package-lock.json covers npm. Each job's workspace setup installs all three
# with the runner's own tool lookup, and the check fails the job unless it did.
JAVASCRIPT_FIXTURES = ROOT / 'crates/motor-runner/test-fixtures/javascript'
JAVASCRIPT_FIXTURE_MANAGERS = ('bun', 'pnpm')
NODE_CHECK = ("for m in " + " ".join(JAVASCRIPT_FIXTURE_MANAGERS) + "; do "
              "test -f \"$m/node_modules/fixture-dep/index.js\" && test ! -e \"$m/lifecycle-ran\" "
              "|| { echo \"runner setup did not install the $m fixture\" >&2; exit 1; }; done; "
              "node -e \"if (require('boat-dep') !== 'boat-node-modules') process.exit(1); console.log(require('boat-dep'))\"")


def job_repository(repository, cargo_seed_manifest=None):
    """A local tarball installs regular files in node_modules, unlike a local
    directory dependency that leaves a symlink. Publishing and reusing
    node_modules must therefore reflink those bytes."""
    (repository / 'src').mkdir()
    (repository / 'Cargo.toml').write_text('[package]\nname="boat-hello"\nversion="0.1.0"\nedition="2021"\n')
    (repository / 'Cargo.lock').write_text('version = 4\n[[package]]\nname = "boat-hello"\nversion = "0.1.0"\n')
    (repository / 'src/main.rs').write_text('fn main() { println!("boat-hello"); }\n')
    if cargo_seed_manifest:
        shutil.copyfile(cargo_seed_manifest, repository / 'cargo-seed-expected.json')
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
    for name in JAVASCRIPT_FIXTURE_MANAGERS:
        shutil.copytree(JAVASCRIPT_FIXTURES / name, repository / name, ignore=shutil.ignore_patterns('node_modules'))
    for command in (['git','init','-q'], ['git','add','.'],
                    ['git','-c','user.name=Boat probe','-c','user.email=probe@invalid','commit','-qm','probe input']):
        subprocess.run(command, cwd=repository, check=True, stdout=sys.stderr)


def git_pushurl_check(parsed):
    """(passed, evidence) for the job that resets a pushurl at worktree scope.
    The probe prints its JSON report whether or not the job's git passes."""
    try:
        report = json.loads(parsed['stdout'])
    except ValueError:
        return False, parsed
    return parsed['code'] == 0 and report.get('passed') is True, report


def wasm_opt_check(parsed):
    try:
        report = json.loads(parsed['stdout'])
    except ValueError:
        return False, parsed
    def valid_digest(value):
        return isinstance(value, str) and len(value) == 64 and all(
            character in '0123456789abcdef' for character in value)
    version = re.match(r'wasm-opt version (\d+)(?:\s|$)', report.get('version', ''))
    passed = (parsed.get('code') == 0 and report.get('passed') is True
              and version is not None and version.group(1) == '120'
              and report.get('round_trip_bytes', 0) > 0
              and valid_digest(report.get('module_sha256'))
              and valid_digest(report.get('round_trip_sha256')))
    return passed, report


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


def isolation_result_checks(result, host, weight=None):
    if result['code'] != 0:
        return {'isolation_job_completed': False}
    try:
        payload = json.loads(result['stdout'])
    except ValueError as error:
        raise ValueError('isolation job exited zero but did not emit JSON evidence') from error
    return isolation_checks(payload, host, weight)


def isolation_checks(job, host, weight=None):
    """`weight` is the vCPUs the job asked for; by default the sandbox's measured CPUs."""
    cpu = job["limits"]["cpu.max"].split()
    budget = host['memory_budget']
    usable = usable_vcpus(host)
    weight = usable if weight is None else weight
    cpu_expected = [str(weight * 100000), "100000"] if weight else None
    expected = (budget['mem_available_bytes'] - budget['runner_idle_bytes'] - budget['margin_bytes']) // (1 << 20) * (1 << 20)
    return {
        "job_account": job["user"] == "motor-job" and job["uid"] != 0,
        "loopback_only": job["interfaces"] == ["lo"],
        "private_network": job["namespaces"]["net"] != host["namespaces"]["net"],
        "private_shm": job["private_shm"] is True and job["shm_writable"] is True,
        # The 1 GiB floor is the start wrapper's and the runner's own refusal floor, the same on
        # every size; the budget itself is what this sandbox measured.
        "memory_limit": job["limits"]["memory.max"] == str(budget['memory_max_bytes']) and budget['memory_max_bytes'] == expected and expected > (1 << 30),
        # The runner sets CPUQuota to the job's requested vCPUs; nproc is the sandbox's CPUs.
        "cpu_limit": usable is not None and len(cpu) == 2 and cpu == cpu_expected and job['nproc'] == str(usable),
        "pid_limit": job["limits"]["pids.max"] == "4096",
        "no_provider_job_environment": not any(job["provider_environment"].values()),
        "no_provider_job_access": provider_access_denied(job['provider_access']),
        "no_privilege_gain": job['status_security']['NoNewPrivs'] == '1' and int(job['status_security']['CapEff'],16) == 0 and int(job['status_security']['CapBnd'],16) == 0,
        **job['execution_capabilities']['checks'],
        **process_visibility_checks(job),
        **proc_checks(job),
        **pid_checks(job, host),
        # Rust jobs build in the build server's fixed folder, bound to ./target.
        "fixed_cargo_target_dir": job['cargo_target'].get('env') == '/run/motor/target' and job['cargo_target'].get('same_directory') is True,
    }


def process_visibility_checks(job):
    visibility = job['process_visibility']
    proc = visibility['proc']
    ps = visibility['ps']
    protected = set(visibility['protected_pids'])
    signal = visibility['runner_signal_probe']
    return {
        'proc_only_job_processes': bool(proc) and any(row['pid'] == visibility['self_pid'] for row in proc) and
            all(row.get('uid') == job['uid'] and row.get('same_cgroup') for row in proc),
        'ps_only_job_processes': ps['exit_code'] == 0 and bool(ps['rows']) and
            any(row['pid'] == visibility['self_pid'] for row in ps['rows']) and
            all(row['uid'] == job['uid'] for row in ps['rows']),
        'cannot_signal_runner': signal['pid'] in protected and outside_control_passes(signal, job['uid'], job['pid_namespace']['self_cgroup']),
    }


def proc_checks(job):
    proc = job['proc_global']
    options = [line.split(' - ', 1)[1].split()[2].split(',') for line in job['proc_mountinfo']
               if ' - ' in line and line.split()[4] == '/proc']
    return {
        # Other accounts' processes stay hidden, but the mount is not limited to
        # process entries: ProcSubset=pid (subset=pid) hides /proc/stat from ps.
        'proc_hides_other_users': bool(options) and all(
            ('hidepid=invisible' in mount or 'hidepid=2' in mount) and 'subset=pid' not in mount for mount in options),
        'proc_stat_has_btime': proc['stat'].get('readable') is True and proc['stat'].get('has_line') is True,
        'proc_meminfo_readable': proc['meminfo'].get('readable') is True and proc['meminfo'].get('has_line') is True,
        'proc_locks_readable': proc.get('locks', {}).get('readable') is True,
        'proc_net_unix_readable': proc.get('net_unix', {}).get('readable') is True
            and proc['net_unix'].get('has_line') is True,
        'proc_sys_read_only': proc['sys_read_only'] is True,
        'proc_cmdline_no_provider_names': proc['cmdline'].get('readable') is True and proc['cmdline'].get('provider_names') is False,
    }


def provider_access_denied(access):
    return bool(access['files']) and bool(access['processes']) and not access['inherited_names'] and all(not row.get('ascii_token_assignment') and
        (not path.endswith('/.config/ascii/boat/config.json') or not row['readable'])
        for path,row in access['files'].items()) and all(not row.get('ascii_token_present') for row in access['processes'].values())


def prepared_image_digest(revision, inventory):
    inputs = {'revision':revision, 'packages':inventory['packages'], 'apt_release_files':inventory['apt_release_files'],
              'cargo_seed_sha256': inventory['cargo_seed']['manifest_sha256']}
    return hashlib.sha256(json.dumps(inputs, sort_keys=True, separators=(',',':')).encode()).hexdigest()


def parity_source(job=False, gnu_test_sha256=None, cargo_seed_sha256=None):
    # Run the operator checkout's checker, not an image's possibly stale checker
    # or its build-time inventory. Every phase measures the current filesystem.
    return ("import json, types\n"
            "seed_scope = {'__name__': 'cargo_seed'}\n"
            f"exec({CARGO_SEED_SCRIPT.read_text()!r}, seed_scope)\n"
            "syscall_scope = {'__name__': 'syscall_surface'}\n"
            f"exec({SYSCALL_SURFACE_SCRIPT.read_text()!r}, syscall_scope)\n"
            "scope = {'__name__': 'toolchain_parity', '__file__': '/toolchain-parity.py', 'seed': types.SimpleNamespace(**seed_scope), 'syscall_surface': types.SimpleNamespace(**syscall_scope)}\n"
            f"exec({PARITY_SCRIPT.read_text()!r}, scope)\n"
            f"registry = seed_scope['verified_manifest']({'cargo-seed-expected.json' if job else '/home/user/cargo-seed-expected.json'!r}, {cargo_seed_sha256!r}) if {cargo_seed_sha256!r} else None\n"
            f"result = scope['check'](json.loads({PARITY_MANIFEST.read_text()!r}), role='boat', job={job!r}, gnu_test_sha256={gnu_test_sha256!r}, cargo_registry=registry)\n"
            "print(json.dumps(result))\n")


def parity_passed(report):
    manifest = json.loads(PARITY_MANIFEST.read_text())
    expected = hashlib.sha256(json.dumps(manifest, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    return report.get('passed') is True and report.get('errors') == [] and report.get('manifest_sha256') == expected


def root_paths_source():
    # Like the parity check, run the operator checkout's rule (the one the image
    # recipes enforce at build time) against the live filesystem in every phase.
    return ("import json\nscope = {'__name__': 'root_paths'}\n"
            f"exec({ROOT_PATHS_SCRIPT.read_text()!r}, scope)\n"
            "print(json.dumps(scope['check'](scope['BOAT_ROOTS'], "
            "provider_files=scope['BOAT_PROVIDER_RESTORED'])))\n")


def root_paths_passed(report):
    return report.get('offender_count') == 0 and report.get('offenders') == [] and report.get('entries', 0) > 0


# vCPUs a probe job asks for, by job name, when it needs a fixed count. A job not listed
# asks for every vCPU the sandbox measured, the way one spilled job uses a whole sandbox.
JOB_VCPUS = {}


def usable_vcpus(host):
    """The sandbox's CPUs as nproc measured them in the sandbox, or None when unmeasured."""
    value = (host.get('sizing') or {}).get('nproc')
    return value if type(value) is int and value > 0 else None


def job_vcpus(requested, usable, size):
    """(vCPUs to ask for, None), or (None, reason) when the job cannot fit this size and
    must be skipped: asking the runner for more CPUs than exist measures nothing."""
    if requested is None:
        return usable, None
    if requested > usable:
        return None, (f'skipped: the job asks for {requested} vCPUs and this {size} sandbox '
                      f'has {usable} usable vCPUs')
    return requested, None


def argv_int(argv, flag):
    """The integer after the last `flag` in `argv`, or None."""
    values = [argv[index + 1] for index in range(len(argv) - 1) if argv[index] == flag]
    try:
        return int(values[-1]) if values else None
    except ValueError:
        return None


def meminfo_bytes(text, key):
    for line in (text or '').splitlines():
        name, _, value = line.partition(':')
        if name == key:
            amount, _, unit = value.strip().partition(' ')
            return int(amount) * 1024 if unit == 'kB' and amount.isdigit() else None
    return None


def sizing_record(host, job_weight):
    """What this sandbox measured about its own size, and what the runner and endpoint were
    configured with, so CLOUD can size its broker from real values for each Boat size."""
    sizing = host.get('sizing') or {}
    budget = host.get('memory_budget') or {}
    runner = sizing.get('runner_argv') or []
    return {'usable_vcpus': usable_vcpus(host), 'affinity_cpus': sizing.get('affinity_cpus'),
            'runner_max_weight': argv_int(runner, '--max-weight'),
            'runner_rustc_slots': argv_int(runner, '--rustc-slots'),
            'endpoint_vcpu_ceiling': (sizing.get('endpoint_config') or {}).get('vcpu_ceiling'),
            'endpoint_config_path': sizing.get('endpoint_config_path'),
            'job_vcpus': job_weight,
            'runner_memory_max_bytes': budget.get('memory_max_bytes'),
            'runner_memory_flag_bytes': argv_int(runner, '--boat-memory-max-bytes'),
            'mem_total_bytes': meminfo_bytes(host.get('meminfo_at_idle'), 'MemTotal'),
            'mem_available_before_runner_bytes': budget.get('mem_available_bytes'),
            'mem_available_at_idle_bytes': meminfo_bytes(host.get('meminfo_at_idle'), 'MemAvailable'),
            'runner_idle_bytes': budget.get('runner_idle_bytes'), 'margin_bytes': budget.get('margin_bytes'),
            'state_disk': sizing.get('state_disk')}


def sizing_checks(record):
    """The runner and endpoint must use this sandbox's measured size, not the size the
    image was built on: an image built on large and cloned onto default would otherwise
    admit 8 vCPUs of work onto 4."""
    usable = record['usable_vcpus']
    memory = record['runner_memory_max_bytes']
    return {
        'runner_vcpus_follow_sandbox': usable is not None and record['runner_max_weight'] == usable
            and record['runner_rustc_slots'] == usable and record['endpoint_vcpu_ceiling'] == usable,
        'runner_memory_from_measurement': type(memory) is int and record['runner_memory_flag_bytes'] == memory
            and type(record['mem_total_bytes']) is int and memory < record['mem_total_bytes'],
    }


def sizing_reason(key, record):
    if key == 'runner_vcpus_follow_sandbox':
        return (f"sandbox has {record['usable_vcpus']} usable vCPUs; runner --max-weight "
                f"{record['runner_max_weight']}, --rustc-slots {record['runner_rustc_slots']}, endpoint "
                f"vcpu_ceiling {record['endpoint_vcpu_ceiling']} ({record['endpoint_config_path']})")
    return (f"runner --boat-memory-max-bytes {record['runner_memory_flag_bytes']}, measured budget "
            f"{record['runner_memory_max_bytes']}, MemTotal {record['mem_total_bytes']}")


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

    def check(self, name, passed, evidence, status=None, reason=None):
        row = {"name": name, "status": status or ("PASS" if passed else "FAIL"), "evidence": evidence}
        if row['status'] == 'FAIL':
            row['reason'] = reason or f"required check {name} did not meet its acceptance criteria; observed values are attached as evidence"
        self.data["checks"].append(row)
        self.probe.checkpoint()

    def check_job(self, name, result, expected, *, signal=None, marker=None, exact_stdout=None):
        failure = lifecycle_failure(result, expected, signal=signal, marker=marker, exact_stdout=exact_stdout)
        self.check(name, failure is None, result, reason=failure)

    def execute(self, sid, command, timeout=120):
        result = self.probe.execute(sid, command, poll_seconds=timeout)
        if result.get("exitCode") != 0 or result.get("stdoutTruncated") or result.get("stderrTruncated"):
            failure = {'command': command, 'result': result}
            self.data.setdefault('guest_failures', []).append(failure)
            try:
                diagnostics = self.probe.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(DIAGNOSTICS), poll_seconds=100)
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
        self.execute(sid, 'sudo -n systemctl stop motor-boat-identity motor-boat-host motor-boat-bridge motor-boat motor-boat-storage-watch')
        hygiene = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(SNAPSHOT_HYGIENE) + ' ' + shlex.quote(public))['stdout'])
        self.check(label + '.identity_and_probe_keys_absent', all(hygiene[k] is True for k in ('identity_absent','boat_cli_configs_absent','probe_authorizations_absent')), hygiene)
        self.execute(sid, "sudo -n sync && sudo -n systemctl stop Users.mount 'var-lib-motor\\x2dboat.mount' motor-boat-storage && "
                          'sudo -n sync && '
                          'sudo -n /usr/bin/python3 /usr/local/libexec/motor-boat-start.py --check-mountpoints')
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
        return json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(ENDPOINT_STATE))['stdout'])

    def client_failure_diagnostics(self, sid, result):
        """Record what the sandbox was doing when a real job's client failed
        or timed out without a terminal. Stored on the client result, so it
        lands in results.json beside the failure; never raises, so the
        client's own failure stays the reported error."""
        try:
            collected = self.probe.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(JOB_HANG_DIAGNOSTICS),
                                           poll_seconds=300)
            result['sandbox_diagnostics_command'] = {key: collected.get(key) for key in
                                                     ('exitCode', 'stderr', 'stdoutTruncated', 'stderrTruncated')}
            result['sandbox_diagnostics'] = json.loads(collected['stdout'])
        except Exception as error:
            result['sandbox_diagnostics_error'] = str(error)
        try:
            self.probe.checkpoint()
        except Exception as error:
            result['sandbox_diagnostics_checkpoint_error'] = str(error)

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
        upload_seed_input(argv, self.probe.cargo_seed_manifest, 'cargo-seed-expected.json')
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
        record['kernel_release'] = self.execute(sid, 'uname -r')['stdout'].strip()
        record['storage'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(STORAGE_STATE))['stdout'])
        for key, passed in storage_checks(record['storage']).items():
            self.check(label + '.' + key, passed, record['storage'])
        record['storage_timeline'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(STORAGE_TIMELINE))['stdout'])
        self.check(label + '.no_deferred_cleanup_after_readiness', storage_timeline_checks(record['storage_timeline']), record['storage_timeline'])
        self.execute(sid, 'sudo -n systemctl start motor-boat-bridge')
        record["host"] = json.loads(self.execute(sid, "sudo -n -E python3 -c " + shlex.quote(HOST))["stdout"])
        host = record["host"]
        size = getattr(self.probe, 'size', 'large')
        usable = usable_vcpus(host)
        if usable is None:
            raise ValueError('the sandbox CPU count was not measured; refusing to size probe jobs')
        record['sizing'] = sizing_record(host, usable)
        self.probe.data.setdefault('sizing', {}).setdefault(size, {}).setdefault('phases', {})[label] = record['sizing']
        for key, passed in sizing_checks(record['sizing']).items():
            self.check(label + '.' + key, passed, record['sizing'], reason=None if passed else sizing_reason(key, record['sizing']))
        host['job_security_reference'] = json.loads(self.execute(
            sid, 'sudo -n ' + reference_command())['stdout'])
        record['toolchain_parity'] = json.loads(self.execute(
            sid, "sudo -n /usr/bin/python3 -c " + shlex.quote(parity_source(cargo_seed_sha256=self.probe.cargo_seed_sha256)))['stdout'])
        self.check(label + '.toolchain_parity', parity_passed(record['toolchain_parity']), record['toolchain_parity'])
        record['root_owned_paths'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(root_paths_source()))['stdout'])
        self.check(label + '.root_owned_paths', root_paths_passed(record['root_owned_paths']), record['root_owned_paths'],
                   reason=f"{record['root_owned_paths'].get('offender_count')} path(s) root executes from are not root-owned "
                          f"without group/other write; first: {record['root_owned_paths'].get('offenders')}")
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
        host['outside_pid_controls'] = json.loads(self.execute(sid, control_identity_command(protected))['stdout'])
        identities = host['outside_pid_controls']['live']
        protected = [row['pid'] for row in identities]
        job_script = 'PROVIDER_PATHS=' + repr(host['provider_sanitization']['paths_checked']) + '\nPROVIDER_PIDS=' + repr([p['pid'] for p in host['provider_sanitization']['processes']]) + '\nRUNNER_PID=' + repr(host['runner_pid']) + '\nPROTECTED_PIDS=' + repr(protected) + '\nPROTECTED_IDENTITIES=' + repr(identities) + '\n' + JOB
        with tunnel(argv, directory, self.probe.deadline, record.setdefault("tunnel", {})) as address:
            with tempfile.TemporaryDirectory(prefix="motor-boat-client-") as storage:
                gnu_test_hash = (record['toolchain_parity']['observed']['tools'].get('gnutest') or {}).get('sha256')
                jobs = [("node_modules_first", NODE_CHECK), ("node_modules_reuse", NODE_CHECK),
                                       ("exit", "printf 'boat-round-trip\\n'; exit 23"),
                                        ("rust", 'cargo +1.99.0 build --offline --locked && "$CARGO_TARGET_DIR/debug/boat-hello"'),
                                         ("job_toolchain_parity", "cargo --version >/dev/null && python3 -c " + shlex.quote(parity_source(
                                            job=True, gnu_test_sha256=gnu_test_hash, cargo_seed_sha256=self.probe.cargo_seed_sha256))),
                                         ('offline_cargo_seed', 'python3 -c ' + shlex.quote(CARGO_OFFLINE_PROBE.read_text())),
                                          ('git_worktree_pushurl_reset', 'python3 -c ' + shlex.quote(GIT_PUSHURL_PROBE.read_text())),
                                          ('wasm_opt_120_round_trip', 'python3 -c ' + shlex.quote(WASM_OPT_PROBE.read_text())),
                                         ("isolation", "python3 -c " + shlex.quote(job_script)),
                                        ('sigkill_job', 'kill -KILL $$'),
                                        ('job_stdin_eof', "python3 -c 'import os; print(\"boat-stdin-eof\" if os.read(0, 1) == b\"\" else \"unexpected-input\")'"),
                                        ('pid1_ignores_job_sigkill', 'kill -KILL 1; printf boat-pid1-alive')]
                if label in ('prepared_clone', 'snapshot_clone'):
                    jobs.append(('after_idle', NODE_CHECK))
                for name, command in jobs:
                    weight, skipped = job_vcpus(JOB_VCPUS.get(name), usable, size)
                    if skipped:
                        record.setdefault('skipped_jobs', []).append(
                            {'name': name, 'requested_vcpus': JOB_VCPUS.get(name), 'usable_vcpus': usable, 'reason': skipped})
                        self.probe.checkpoint()
                        continue
                    if name == 'after_idle':
                        idle = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(IDLE_RECLAIM), timeout=1050)['stdout'])
                        record['idle_reclaim'] = idle
                        status = idle_reclaim_status(idle)
                        self.check(label + '.idle_reclaim_observed', status == 'PASS', idle, status=status)
                    result = process([str(client), address, str(repository), storage, command, str(weight)], timeout=min(150, max(1, int(self.probe.deadline-time.monotonic()))))
                    record[name] = result
                    if result.get("exit_code") != 0:
                        failure = "motor client failed: " + json.dumps(result)
                        self.client_failure_diagnostics(sid, record[name])
                        raise ValueError(failure + "; sandbox state at failure: runner_host.phases[]." + name + ".sandbox_diagnostics")
                    parsed = job_result(result["stdout"])
                    record[name]["result"] = parsed
                    if first_started is not None and name == 'node_modules_first' and parsed['code'] == 0 and 'boat-node-modules' in parsed['stdout']:
                        self.data["snapshot_create_to_first_job_seconds"] = time.monotonic() - first_started
                    if name == 'job_toolchain_parity':
                        report = json.loads(parsed['stdout'])
                        self.check(label + '.' + name, parsed['code'] == 0 and parity_passed(report), report)
                    elif name == 'offline_cargo_seed':
                        report = json.loads(parsed['stdout']) if parsed['code'] == 0 else parsed
                        self.check(label + '.' + name, parsed['code'] == 0 and report.get('passed') is True, report)
                    elif name == 'git_worktree_pushurl_reset':
                        self.check(label + '.' + name, *git_pushurl_check(parsed))
                    elif name == 'wasm_opt_120_round_trip':
                        self.check(label + '.' + name, *wasm_opt_check(parsed))
                    elif name == "isolation":
                        checks = isolation_result_checks(parsed, host, weight)
                        isolation = json.loads(parsed['stdout']) if parsed['code'] == 0 and not all(checks.values()) else {}
                        for key, passed in checks.items():
                            reason = None if passed else (pid_failure_reason(key, isolation, host) if parsed['code'] == 0
                                                         else f"expected isolation job code 0; observed {parsed['code']}")
                            self.check(label + "." + key, parsed["code"] == 0 and passed, parsed, reason=reason)
                    elif name == 'sigkill_job':
                        self.check_job(label + '.sigkill_job_exit_137', parsed, 137)
                    elif name == 'pid1_ignores_job_sigkill':
                        self.check_job(label + '.pid1_ignores_job_sigkill', parsed, 0, marker='boat-pid1-alive')
                    elif name == 'job_stdin_eof':
                        self.check_job(label + '.job_stdin_eof', parsed, 0, exact_stdout='boat-stdin-eof')
                    else:
                        expected = 23 if name == "exit" else 0
                        marker = "boat-round-trip" if name == "exit" else ("boat-node-modules" if name.startswith('node_modules_') or name == 'after_idle' else "boat-hello")
                        self.check_job(label + "." + name, parsed, expected, marker=marker)
                    state = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(STORAGE_STATE))['stdout'])
                    record.setdefault('storage_after_jobs', {})[name] = state
                    for key, passed in storage_checks(state).items():
                        self.check(label + '.' + name + '.' + key, passed, state)
                # Observe and stop the unit from the sandbox's root namespace
                # while the client waits, including a child that escaped setsid.
                from concurrent.futures import ThreadPoolExecutor
                for action, expected in [('stop', 143), ('kill', 137)]:
                    marker = 'boat-pid-teardown-' + label + '-' + action
                    command = teardown_job(marker)
                    with ThreadPoolExecutor(max_workers=1) as executor:
                        pending = executor.submit(process, [str(client), address, str(repository), storage, command, str(usable)], timeout=90)
                        observed = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(TEARDOWN_SOURCE)
                                                          + ' ' + shlex.quote(marker) + ' ' + action, timeout=60)['stdout'])
                        completed = pending.result()
                    parsed = job_result(completed['stdout']) if completed.get('exit_code') == 0 else {}
                    record['setsid_' + action] = {'host':observed, 'client':completed, 'result':parsed}
                    self.check(label + '.setsid_' + action + '_reaped', teardown_checks(observed), observed)
                    self.check_job(label + '.setsid_' + action + '_exit_' + str(expected), parsed, expected,
                                   signal=9 if action == 'kill' else None)
                    if host['systemd_version'] < 257:
                        for key, passed in bootstrap_checks(observed, host['job_security_reference']).items():
                            self.check(label + '.setsid_' + action + '.' + key, passed, observed)
        self.check(label + ".tunnel_reaped", record["tunnel"].get("reaped") is True, record["tunnel"])
        if any(check['status'] == 'FAIL' for check in self.data['checks'] if check['name'].startswith(label + '.')):
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
        size = getattr(self.probe, 'size', 'large')
        if not provider_sizing_matches(item, size):
            raise ValueError(sizing_error(size))
        self.probe.wait_hydrated(sid)
        self.data['create_from_snapshot']['seconds_to_hydrated'] = time.monotonic() - started
        with tempfile.TemporaryDirectory(prefix='boat-job-', dir=ROOT / '.cortexkit') as repo:
            repository = Path(repo)
            job_repository(repository, self.probe.cargo_seed_manifest)
            # Do not restart, reconfigure or mount anything in the guest: that
            # would repair the startup order this mode is meant to observe.
            self.phase(sid, 'prepared_clone', directory, public, repository, client, first_started=started)
            self.disk_quota(sid, directory, public, repository, client)
            if not self.data['disk_quota'].get('stopped_with_retained_target'):
                raise ValueError('quota probe did not archive the retained target')
            self.data['disk_quota']['resume'] = self.probe.request('POST', f'/sandboxes/{sid}/resume', {})
            from probe import READY, required_checks_passed
            self.probe.wait_sandbox(sid, READY)
            self.probe.wait_hydrated(sid)
            resumed = self.phase(sid, 'resumed', directory, public, repository, client, hydrated_at=time.monotonic())
            self.check_job('disk_quota.normal_job_after_stop_resume', resumed['node_modules_first']['result'], 0, marker='boat-node-modules')
            self.replay_cleanup(sid)
        self.data['all_passed'] = bool(self.data['checks']) and all(c['status'] == 'PASS' for c in self.data['checks'])
        self.data['required_checks_passed'] = required_checks_passed(self.data['checks'])
        self.probe.checkpoint()

    def replay_cleanup(self, sid):
        result = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(DEFERRED_CLEANUP_REPLAY))['stdout'])
        self.data['deferred_cleanup_replay'] = result
        self.check('deferred_cleanup_replay.no_mount_over_live_workspace', cleanup_replay_checks(result), result)

    def disk_quota(self, sid, directory, public, repository, client):
        import disk_quota
        import quota_adversarial
        import uuid
        record: dict[str, Any] = {'reserve_assurance': {
            'floor_bytes': disk_quota.RESERVE_BYTES, 'scope': 'floor_enforcement_only', 'sufficiency_proven': False}}
        self.data['disk_quota'] = record
        control = record['setter_positive_control'] = json.loads(self.execute(
            sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(disk_quota.POSITIVE_CONTROL))['stdout'])
        self.check('disk_quota.fssetxattr_positive_control', disk_quota.positive_control_passed(control), control)
        if not disk_quota.positive_control_passed(control):
            raise ValueError('unfiltered XFS attribute positive control failed')
        argv = self.route(sid, directory, public)
        with tunnel(argv, directory, self.probe.deadline, record.setdefault('tunnel', {})) as address:
            with tempfile.TemporaryDirectory(prefix='boat-quota-client-') as storage:
                def simple_job(name, source, marker=None):
                    result = process([str(client), address, str(repository), storage, 'python3 -c ' + shlex.quote(source), '1', '900'], timeout=1000)
                    parsed = job_result(result['stdout']) if result.get('exit_code') == 0 else {}
                    record[name] = {'client': result, 'result': parsed}
                    self.check_job('disk_quota.' + name, parsed, 0, marker=marker)
                    if parsed.get('code') != 0:
                        raise ValueError('adversarial setup job failed: ' + name)
                    return parsed
                simple_job('deep_tree', disk_quota.DEEP_JOB)
                record['deep_cleanup'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(disk_quota.CLEANUP), timeout=100)['stdout'])
                self.check('disk_quota.deep_tree_capacity_recovered', record['deep_cleanup'].get('ready') is True and record['deep_cleanup'].get('leases') == [], record['deep_cleanup'])
                for lane in disk_quota.LANES + (disk_quota.RETAINED_LANE,):
                    if lane == 'reused_target':
                        simple_job('baseline_seed', disk_quota.BASELINE_JOB, marker='baseline-seeded')
                    if lane == 'reflink_target':
                        capacity = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(disk_quota.CAPACITY))['stdout'])
                        record['reflink_seed_capacity'] = capacity
                        if capacity['baseline_bytes'] < 64 * 1024**2:
                            self.check('disk_quota.reflink_baseline_headroom', False, capacity, status='INCONCLUSIVE')
                            continue
                        simple_job('reflink_seed', disk_quota.reflink_seed(capacity['baseline_bytes']))
                    row: dict[str, Any] = {}
                    record.setdefault('lanes', {})[lane] = row
                    token = uuid.uuid4().hex
                    command = 'python3 -c ' + shlex.quote(disk_quota.job(lane, token))
                    invocation = [str(client), address, str(repository), storage, command, '1', '900']
                    started = time.monotonic()
                    child = _quota_popen(invocation, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                             text=True, env={key: value for key, value in os.environ.items() if key != 'BOAT_API_KEY'})
                    complete = False
                    try:
                        row['at_limit'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(disk_quota.observer(token, lane == disk_quota.RETAINED_LANE)), timeout=950)['stdout'])
                        if lane == disk_quota.RETAINED_LANE:
                            self.check('disk_quota.retained_target_at_limit', disk_quota.retained_checks(row['at_limit']), row['at_limit'])
                            if not disk_quota.retained_checks(row['at_limit']):
                                raise ValueError('retained target did not reach an enforced quota boundary')
                            row['pre_stop'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(quota_adversarial.retained_guard(row['at_limit'])))['stdout'])
                            if row['pre_stop'].get('job_running') is not True:
                                raise ValueError('retained target job is no longer live before snapshot')
                            # Keep the job at its barrier so idle eviction cannot
                            # remove the full target before the provider snapshots it.
                            record['stop'] = self.probe.request('POST', f'/sandboxes/{sid}/stop', {})
                            self.probe.wait_sandbox(sid, {'archived'})
                            record['stopped_with_retained_target'] = True
                            row['client_interrupted_for_snapshot'] = True
                            complete = True
                            self.check('disk_quota.stop_with_quota_sized_target', True, row['at_limit'])
                            break
                        if 'errno' not in row['at_limit']:
                            raise ValueError('quota job ended before reaching its write limit')
                        stdout, stderr = child.communicate(timeout=1000)
                        result = dict(argv=invocation, exit_code=child.returncode, stdout=stdout, stderr=stderr, seconds=time.monotonic()-started)
                        row['client'] = result
                        row['result'] = job_result(stdout) if child.returncode == 0 else {}
                        row['cleanup'] = json.loads(self.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(disk_quota.CLEANUP), timeout=100)['stdout'])
                        for name, passed in disk_quota.checks(row['result'], row['at_limit'], row['cleanup']).items():
                            self.check('disk_quota.' + lane + '.' + name, passed, row)
                        status = disk_quota.file_setattr_status(row['result'], control)
                        reason = ('unfiltered root file_setattr returned ENOSYS; this cannot distinguish the job filter; uname -r: '
                                  + control.get('kernel_release', 'unknown')) if status == 'INCONCLUSIVE' else None
                        self.check('disk_quota.' + lane + '.file_setattr_refusal', status == 'PASS',
                                   {'control': control, 'job': row['result'], 'observation_gap': reason}, status=status, reason=reason)
                        if lane == 'tmp': record['at_limit'] = row['at_limit']
                        if any(check['status'] == 'FAIL' for check in self.data['checks'] if check['name'].startswith('disk_quota.' + lane + '.')):
                            raise ValueError('disk quota lane failed: ' + lane)
                        normal = process([str(client), address, str(repository), storage, 'printf boat-after-quota', '1'], timeout=120)
                        row['next_job'] = normal
                        self.check_job('disk_quota.' + lane + '.normal_job_after_cleanup', job_result(normal['stdout']) if normal.get('exit_code') == 0 else {}, 0, exact_stdout='boat-after-quota')
                        if any(check['status'] == 'FAIL' for check in self.data['checks'] if check['name'].startswith('disk_quota.' + lane + '.')):
                            raise ValueError('normal job after quota cleanup failed: ' + lane)
                        complete = True
                    finally:
                        captured = None
                        if child.poll() is None:
                            child.terminate()
                            try: captured = child.communicate(timeout=5)
                            except subprocess.TimeoutExpired:
                                child.kill(); captured = child.communicate()
                        if row.get('client_interrupted_for_snapshot'):
                            stdout, stderr = captured if captured is not None else child.communicate()
                            row['client'] = dict(exit_code=child.returncode, stdout=stdout, stderr=stderr,
                                                 seconds=time.monotonic()-started, intentionally_interrupted=True)
                        if not complete:
                            try:
                                row['abort_cleanup'] = self.probe.execute(sid, 'sudo -n /usr/bin/python3 -c ' + shlex.quote(quota_adversarial.abort_source(token)), poll_seconds=30)
                            except Exception as error:
                                row['abort_cleanup_error'] = str(error)
                if not record.get('stopped_with_retained_target'):
                    raise ValueError('no retained-target snapshot qualification was performed')
        if any(check['status'] == 'FAIL' for check in self.data['checks'] if check['name'].startswith('disk_quota.')):
            raise ValueError('disk quota qualification failed')

    def _run(self):
        from probe import READY, cleanup, required_checks_passed
        if hashlib.sha256(self.probe.cargo_seed_manifest.read_bytes()).hexdigest() != self.probe.cargo_seed_sha256:
            raise ValueError('operator Cargo seed manifest SHA-256 mismatch')
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
            if not provider_sizing_matches(item, getattr(self.probe, 'size', 'large')):
                raise ValueError(sizing_error(getattr(self.probe, 'size', 'large')))
            if source_image:
                self.probe.wait_hydrated(sid)
                # A prepared image from an older recipe still has the path unit.
                self.execute(sid, 'sudo -n systemctl stop motor-boat-host motor-boat-bridge motor-boat motor-boat-storage-watch && '
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
            self.data["apt_preflight"] = json.loads(self.execute(sid, "sudo -n /usr/bin/python3 /srv/build/source/script/boat-image/install.py --apt-preflight-only", timeout=180)["stdout"])
            self.probe.checkpoint()
            upload_seed_input(argv, self.probe.cargo_seed_manifest, 'cargo-seed.json')
            upload_seed_input(argv, self.probe.cargo_seed_archive, 'cargo-seed.tar.gz')
            seed_args = (' --cargo-seed-manifest /home/user/cargo-seed.json'
                         ' --cargo-seed-archive /home/user/cargo-seed.tar.gz'
                         ' --cargo-seed-sha256 ' + shlex.quote(self.probe.cargo_seed_sha256)
                         + ' --acknowledge-tenant-one-private-cache')
            self.data["install"] = self.execute(sid, f"sudo -n /usr/bin/python3 /srv/build/source/script/boat-image/install.py --revision {revision}" + seed_args, timeout=max(1, int(self.probe.deadline-time.monotonic())-120))
            self.execute(sid, 'rm -f /home/user/cargo-seed.tar.gz /home/user/cargo-seed.json')
            self.data["install_seconds"] = time.monotonic() - started
            if getattr(self.probe, 'retain_prepared_image', False):
                inventory = json.loads(self.execute(sid, 'sudo -n cat /var/lib/motor-boat/inventory.json')['stdout'])
                self.retain_prepared(sid, public, revision, inventory)
            with tempfile.TemporaryDirectory(prefix="boat-job-", dir=ROOT / ".cortexkit") as repo:
                repository = Path(repo)
                job_repository(repository, self.probe.cargo_seed_manifest)
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
                clone, self.data["create_from_snapshot"] = self.probe.create(name, source_size=getattr(self.probe, 'size', 'large'))
                sid = clone["id"]
                # Install while Boat may still be restoring files, as a broker
                # racing the restore would; broker_order re-installs if lost.
                pre_swap = self.install_identity(sid)
                self.probe.wait_hydrated(sid)
                hydrated_at = time.monotonic()
                self.data['create_from_snapshot']['seconds_to_hydrated'] = hydrated_at - start
                self.phase(sid, "snapshot_clone", directory, public, repository, client, first_started=start,
                           pre_swap_install=pre_swap, hydrated_at=hydrated_at)
                self.disk_quota(sid, directory, public, repository, client)
                if not self.data['disk_quota'].get('stopped_with_retained_target'):
                    raise ValueError('quota probe did not archive the retained target')
                self.data['disk_quota']['resume'] = self.probe.request("POST", f"/sandboxes/{sid}/resume", {})
                self.probe.wait_sandbox(sid, READY)
                self.probe.wait_hydrated(sid)
                resumed = self.phase(sid, "resumed", directory, public, repository, client, hydrated_at=time.monotonic())
                self.check_job('disk_quota.normal_job_after_stop_resume', resumed['node_modules_first']['result'], 0, marker='boat-node-modules')
                self.replay_cleanup(sid)
                before = self.data["phases"][1]["host"]["ssh_config"]
                after = self.data["phases"][2]["host"]["ssh_config"]
                self.check("sshd.dropin_survives_resume", before.get("/etc/ssh/sshd_config.d/00-motor-boat.conf") == after.get("/etc/ssh/sshd_config.d/00-motor-boat.conf") and bool(after.get("/etc/ssh/sshd_config.d/00-motor-boat.conf")), {"before": before, "after": after})
        self.data["local_private_keys_removed"] = not directory.exists()
        self.data["all_passed"] = bool(self.data["checks"]) and all(c["status"] == "PASS" for c in self.data["checks"])
        self.data['required_checks_passed'] = required_checks_passed(self.data['checks'])
        if self.data.get('prepared_image'):
            self.data['prepared_image']['real_jobs_qualified'] = self.data['required_checks_passed']
        self.probe.checkpoint()
