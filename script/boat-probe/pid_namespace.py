"""Live evidence and fail-closed checks for the Boat job PID bootstrap."""
import errno

# Shared by the ordinary User=motor-job reference unit and the real job. The
# reference records systemd's credentials and protected proc mounts, not expected
# values reconstructed by the bootstrap being tested.
SECURITY_SOURCE = r'''
import ctypes, errno, os, pathlib, subprocess

def security_evidence():
    def status(text):
        return {line.split(':', 1)[0]: line.split(':', 1)[1].split()
                for line in text.splitlines() if ':' in line}
    own = status(pathlib.Path('/proc/self/status').read_text())
    child = subprocess.run(['/bin/cat', '/proc/self/status'], capture_output=True, text=True, timeout=10)
    mounts = []
    for line in pathlib.Path('/proc/self/mountinfo').read_text().splitlines():
        fields = line.split()
        if fields[4].startswith('/proc/'):
            mounts.append([fields[4], 'ro' in fields[5].split(',')])
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    # KEYCTL_GET_KEYRING_ID(session, create=false), then KEYCTL_DESCRIBE.
    key = libc.syscall(250, 0, -3, 0)
    buf = ctypes.create_string_buffer(4096)
    n = libc.syscall(250, 6, key, buf, len(buf)) if key >= 0 else -1
    keyring = {'id':key, 'errno':ctypes.get_errno() if n < 0 else None}
    if n > 0:
        fields = buf.value.decode().split(';', 4)
        keyring.update(uid=int(fields[1]), gid=int(fields[2]))
    attempts = {}
    for name, flag in [('pid', 0x20000000), ('user', 0x10000000), ('mount', 0x20000),
                       ('net', 0x40000000), ('ipc', 0x8000000), ('uts', 0x4000000), ('cgroup', 0x2000000)]:
        ctypes.set_errno(0)
        result = libc.syscall(272, flag)
        attempts[name] = {'result':result, 'errno':ctypes.get_errno()}
    for name, flag in [('setns_pid', 0x20000000), ('setns_any', 0)]:
        ctypes.set_errno(0)
        result = libc.syscall(308, -1, flag)
        attempts[name] = {'result':result, 'errno':ctypes.get_errno()}
    clone_args = (ctypes.c_ulonglong * 11)()
    clone_args[0] = 0x20000000
    ctypes.set_errno(0)
    result = libc.syscall(435, ctypes.byref(clone_args), ctypes.sizeof(clone_args))
    attempts['clone3_pid'] = {'result':result, 'errno':ctypes.get_errno()}
    return {'status':own, 'fresh_child':status(child.stdout) if child.returncode == 0 else {},
            'proc_submounts':sorted(mounts), 'keyring':keyring, 'namespace_attempts':attempts}
'''

PID_SOURCE = SECURITY_SOURCE + r'''
def inherited_fds():
    kept = {}
    for fd in os.listdir('/proc/self/fd'):
        try:
            kept[fd] = os.readlink('/proc/self/fd/' + fd)
        except FileNotFoundError:
            pass
    return kept

def process_identity(pid):
    directory = pathlib.Path('/proc', str(pid))
    status = (directory/'status').read_text()
    # comm may contain spaces or parentheses; stat's fixed fields begin only
    # after its final closing parenthesis. starttime is field 22 (index 19).
    fields = (directory/'stat').read_text().rsplit(')', 1)[1].split()
    return {'pid':pid, 'uid':int(next(line.split()[1] for line in status.splitlines() if line.startswith('Uid:'))),
            'cgroup':(directory/'cgroup').read_text(), 'comm':(directory/'comm').read_text().strip(),
            'starttime':int(fields[19])}

def pid_evidence(protected):
    init = {}
    for name in ('cgroup', 'comm', 'status'):
        try:
            init[name] = pathlib.Path('/proc/1', name).read_text()
        except OSError as error:
            init[name + '_errno'] = error.errno
    signals = []
    for host in protected:
        pid = host['pid']
        row = {'pid':pid, 'host':host}
        try:
            os.kill(pid, 0)
            row['allowed'] = True
        except OSError as error:
            row.update(allowed=False, errno=error.errno)
        try:
            row['target'] = process_identity(pid)
        except OSError as error:
            row['target_errno'] = error.errno
        target = row.get('target', {})
        row['alias'] = (target.get('uid') == os.getuid() and target.get('cgroup') == pathlib.Path('/proc/self/cgroup').read_text()
                        and target.get('starttime') != host['starttime'])
        signals.append(row)
    return {'init':init, 'self_cgroup':pathlib.Path('/proc/self/cgroup').read_text(),
            'pid':os.getpid(), 'ppid':os.getppid(), 'signals':signals,
            'security':security_evidence()}
'''


def control_identity_command(pids):
    import shlex
    source = PID_SOURCE + '\nimport json\nlive, missing = [], []\n'
    source += 'for pid in ' + repr(sorted(set(pids))) + ':\n'
    source += '    try:\n        live.append(process_identity(pid))\n    except OSError as error:\n        missing.append({"pid":pid,"errno":error.errno})\n'
    source += 'print(json.dumps({"live":live,"missing":missing}))\n'
    return 'sudo -n python3 -c ' + shlex.quote(source)


def outside_control_passes(row, uid, cgroup):
    host = row.get('host', {})
    if not all(key in host for key in ('pid', 'uid', 'cgroup', 'comm', 'starttime')) or host['pid'] != row.get('pid'):
        return False
    if row.get('allowed') is False and row.get('errno') == errno.ESRCH and row.get('target_errno') == errno.ENOENT:
        return True
    target = row.get('target', {})
    return (row.get('allowed') is True and row.get('alias') is True and target.get('uid') == uid
            and target.get('cgroup') == cgroup and isinstance(target.get('starttime'), int)
            and target['starttime'] != host['starttime'])

REFERENCE_PROPERTIES = [
    'User=motor-job', 'Group=motor-job', 'NoNewPrivileges=yes', 'CapabilityBoundingSet=',
    'RestrictNamespaces=yes', 'ProtectProc=invisible', 'ProtectKernelTunables=yes',
    'ProtectKernelModules=yes', 'ProtectControlGroups=yes', 'PrivateDevices=yes',
    'PrivateNetwork=yes', 'PrivateTmp=yes', 'ProtectSystem=strict', 'ProtectHome=tmpfs',
    'MountAPIVFS=yes', 'KeyringMode=private',
]


def reference_command():
    import shlex
    source = SECURITY_SOURCE + '\nimport json\nprint(json.dumps(security_evidence()))\n'
    return shlex.join(['systemd-run', '--quiet', '--wait', '--pipe',
                       *['--property=' + p for p in REFERENCE_PROPERTIES],
                       '/usr/bin/python3', '-c', source])


def pid_checks(job, host):
    evidence = job.get('pid_namespace', {})
    init = evidence.get('init', {})
    security = evidence.get('security', {})
    own = security.get('status', {})
    reference = host.get('job_security_reference', {})
    expected = reference.get('status', {})
    identity = ('Uid', 'Gid', 'Groups')
    caps = ('CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb')
    signals = evidence.get('signals', [])
    attempts = security.get('namespace_attempts', {})
    required = {'pid', 'user', 'mount', 'net', 'ipc', 'uts', 'cgroup', 'setns_pid', 'setns_any', 'clone3_pid'}
    fresh = security.get('fresh_child', {})
    keyring = security.get('keyring', {})
    return {
        'boat_pid_profile': job['namespaces']['pid'] != host['namespaces']['pid'],
        'pid1_cgroup_readable': bool(init.get('cgroup')),
        'pid1_same_cgroup': bool(init.get('cgroup')) and init['cgroup'] == evidence.get('self_cgroup'),
        'pid1_is_wrapper': init.get('comm', '').strip() == 'bash' and 'PPid:\t0\n' in init.get('status', ''),
        'job_pid_is_small': evidence.get('pid') == 2 and evidence.get('ppid') == 1,
        'outside_pids_esrch_or_proven_alias': bool(signals) and {s['pid'] for s in signals} == set(job['process_visibility']['protected_pids'])
            and all(outside_control_passes(s, job['uid'], evidence.get('self_cgroup')) for s in signals),
        'job_credentials_match_systemd': all(own.get(k) and own[k] == expected.get(k) for k in identity)
            and own['Uid'] == [str(job['uid'])] * 4,
        'all_job_capabilities_empty': all(own.get(k) and all(int(v, 16) == 0 for v in own[k]) for k in caps),
        'job_no_new_privileges': own.get('NoNewPrivs') == ['1'],
        'job_namespace_filter': own.get('Seccomp') == ['2']
            and (host.get('systemd_version', 0) >= 257 or
                 int(own.get('Seccomp_filters', ['0'])[0]) > int(expected.get('Seccomp_filters', ['0'])[0]))
            and set(attempts) == required
            and all(a.get('result') == -1 and a.get('errno') == (errno.ENOSYS if name == 'clone3_pid' else errno.EPERM)
                    for name, a in attempts.items()),
        'job_clone3_pid_denied': attempts.get('clone3_pid') == {'result':-1, 'errno':errno.ENOSYS},
        'job_inherited_fds': set(evidence.get('inherited_fds', {})) == {'0', '1', '2'},
        'proc_protections_match_systemd': bool(security.get('proc_submounts'))
            and security['proc_submounts'] == reference.get('proc_submounts'),
        'job_signal_state_matches_systemd': all(fresh.get(k) and fresh[k] == reference.get('fresh_child', {}).get(k)
                                               for k in ('SigBlk', 'SigIgn')),
        'session_keyring_owned_by_job': keyring.get('id', -1) > 0 and keyring.get('uid') == job['uid']
            and own.get('Gid') and keyring.get('gid') == int(own['Gid'][0])
            and keyring['id'] != reference.get('keyring', {}).get('id'),
    }


def lifecycle_checks(result, expected):
    outcome = result.get('terminal', {}).get('outcome', {})
    return result.get('code') == expected and outcome == {'kind':'exit', 'code':expected}


# The host runs this while the production client waits on a real job. Match an
# exact argv[0] owned by motor-job, never the root observer's command line.
TEARDOWN_SOURCE = r'''
import json, os, pathlib, pwd, stat, subprocess, sys, time
marker, action = sys.argv[1:]
uid = pwd.getpwnam('motor-job').pw_uid

def children():
    found = []
    for path in pathlib.Path('/proc').iterdir():
        if path.name.isdecimal():
            try:
                if path.stat().st_uid == uid and (path/'cmdline').read_bytes().split(b'\0')[0].decode() == marker:
                    found.append(int(path.name))
            except (OSError, UnicodeError):
                pass
    return found

end = time.monotonic() + 30
pids = children()
while not pids and time.monotonic() < end:
    time.sleep(.1)
    pids = children()
result = {'found':pids, 'action':action}
if pids:
    pid = pids[0]
    result['setsid'] = os.getsid(pid) == pid
    group = pathlib.Path('/proc', str(pid), 'cgroup').read_text().strip().split('0::', 1)[1]
    unit = group.rsplit('/', 1)[1]
    if not unit.startswith('motor-job-') or not unit.endswith('.service'):
        raise ValueError('unexpected job cgroup: ' + group)
    result['unit'] = unit
    main = int(subprocess.check_output(['systemctl', 'show', unit, '-p', 'MainPID', '--value'], text=True))
    root = pathlib.Path(subprocess.check_output(['systemctl', 'show', unit, '-p', 'RootDirectory', '--value'], text=True).strip())
    record = root.parent/'output/setup-error'
    try:
        meta = record.stat()
        result['bootstrap_record_trusted'] = meta.st_uid == 0 and stat.S_IMODE(meta.st_mode) == 0o600
        with record.open() as stream:
            result['bootstrap'] = json.loads(stream.read(4096))
    except (OSError, ValueError) as error:
        result['bootstrap_error'] = type(error).__name__
    members = (pathlib.Path('/sys/fs/cgroup')/group.lstrip('/')/'cgroup.procs').read_text().split()
    wrappers = []
    for member in members:
        try:
            status = pathlib.Path('/proc', member, 'status').read_text()
            nspid = next(line.split()[1:] for line in status.splitlines() if line.startswith('NSpid:'))
            if nspid[-1] == '1' and int(member) != main:
                wrappers.append(int(member))
        except (OSError, StopIteration):
            pass
    result['separate_session'] = (len(wrappers) == 1 and os.getsid(wrappers[0]) == wrappers[0]
        and os.getpgid(wrappers[0]) == wrappers[0] and os.getsid(main) != os.getsid(wrappers[0])
        and os.getpgid(main) != os.getpgid(wrappers[0]))
    result['kill_mode'] = subprocess.check_output(['systemctl', 'show', unit, '-p', 'KillMode', '--value'], text=True).strip()
    argv = ['systemctl', 'stop', unit] if action == 'stop' else ['systemctl', 'kill', '--kill-whom=all', '--signal=SIGKILL', unit]
    result['control_exit'] = subprocess.run(argv, capture_output=True, timeout=10).returncode
    end = time.monotonic() + 10
    while children() and time.monotonic() < end:
        time.sleep(.1)
    result['survivors'] = children()
print(json.dumps(result))
'''


def teardown_checks(evidence):
    return bool(evidence.get('found')) and evidence.get('setsid') is True and evidence.get('kill_mode') == 'control-group' \
        and evidence.get('control_exit') == 0 and evidence.get('survivors') == []


def bootstrap_checks(evidence, reference):
    state = evidence.get('bootstrap', {})
    trusted = evidence.get('bootstrap_record_trusted') is True
    return {
        'post_drop_pdeathsig': trusted and state.get('pdeathsig') == 9
            and isinstance(state.get('uid'), int) and state['uid'] > 0
            and reference.get('status', {}).get('Uid') == [str(state['uid'])] * 4,
        'private_session_and_pgid': trusted and state.get('sid') == 1 and state.get('pgid') == 1
            and evidence.get('separate_session') is True,
        'bootstrap_exec_fds': trusted and state.get('exec_fds') == [0, 1, 2],
        'bootstrap_expected_submounts': trusted and bool(state.get('proc_submounts'))
            and state['proc_submounts'] == reference.get('proc_submounts'),
    }


def teardown_job(marker):
    import shlex
    # The escaped child ignores TERM, exercising KillMode's final SIGKILL. Its
    # exec keeps the setsid PID while giving the observer a unique argv[0].
    child = 'trap "" TERM; exec -a ' + shlex.quote(marker) + ' /bin/sleep 120'
    return 'setsid /bin/bash -c ' + shlex.quote(child) + ' </dev/null >/dev/null 2>&1 & wait'
