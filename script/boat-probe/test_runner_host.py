import json
import importlib.util
import contextlib
import io
import errno
from unittest.mock import patch, Mock
from pathlib import Path
import sys
import tempfile
import tarfile
import base64
import hashlib
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
try:
    spec = importlib.util.spec_from_file_location('boat_runner_host', Path(__file__).with_name('runner_host.py'))
    if spec is None or spec.loader is None:
        raise ImportError('cannot load Boat runner host probe')
    host = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(host)
finally:
    sys.path.pop(0)


class RunnerHostTests(unittest.TestCase):
    def test_successful_service_without_proven_reclaim_is_inconclusive(self):
        data = {'seconds': 1020.0268031730001, 'started': 267.587254226,
                'before': None, 'seeded': '25618 211478 35468037632 4613912571 0 -900\n',
                'after': '30846 998171 36174351872 4627422811 2 619\n',
                'unit': 'Result=success\nExecMainStartTimestampMonotonic=1252467097\n',
                'reclaimed_during_idle': False}
        self.assertFalse(host.idle_reclaim_checks(data))
        self.assertEqual(host.idle_reclaim_status(data), 'INCONCLUSIVE')
        self.assertEqual(host.idle_reclaim_status(dict(data, unit='Result=exit-code\n')), 'FAIL')
        self.assertEqual(host.idle_reclaim_status(dict(data, after='malformed')), 'FAIL')
        self.assertEqual(host.idle_reclaim_status(dict(data, after='30846 998171 36174351872 4627422811 6 619\n',
                                                       reclaimed_during_idle=True)), 'PASS')

    def test_parity_report_cannot_qualify_on_missing_target_or_stale_manifest(self):
        manifest = json.loads(host.PARITY_MANIFEST.read_text())
        digest = hashlib.sha256(json.dumps(manifest, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        report = {'passed': True, 'errors': [], 'manifest_sha256': digest}
        self.assertTrue(host.parity_passed(report))
        self.assertFalse(host.parity_passed(dict(report, passed=False, errors=['wasm32-unknown-unknown missing'])))
        self.assertFalse(host.parity_passed(dict(report, manifest_sha256='old image manifest')))
        self.assertFalse(host.parity_passed({}))

    def test_parity_checker_remeasures_live_files_and_uses_operator_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp, 'wasm32-unknown-unknown')
            target.mkdir()
            checker = Path(tmp, 'checker.py')
            checker.write_text('from pathlib import Path\ndef check(manifest, role, job, gnu_test_sha256=None, cargo_registry=None):\n'
                               f'    return {{"passed": Path({str(target)!r}).is_dir(), "job": job}}\n')
            with patch.object(host, 'PARITY_SCRIPT', checker):
                source = host.parity_source(job=True)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
            self.assertTrue(json.loads(output.getvalue())['passed'])
            target.rmdir()
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
            self.assertFalse(json.loads(output.getvalue())['passed'])
            phase = __import__('inspect').getsource(host.RunnerHost.phase)
            self.assertIn("label + '.toolchain_parity'", phase)
            self.assertIn('("job_toolchain_parity", "cargo --version', phase)

    def test_idle_reclaim_seeds_missing_state_via_service_and_observes_reclaim(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = Path(tmp, 'reclaim')
            now = [1000.0]
            def service(command, **kwargs):
                if command[-1] == 'ascii-idle-reclaim.service':
                    state.write_text('1 2 3 4 0 -900')
            def pause(seconds):
                now[0] += seconds
                state.write_text(f'2 3 4 5 6 {int(now[0])}')
            output = io.StringIO()
            script = host.IDLE_RECLAIM.replace('/run/ascii-idle-reclaim', str(state))
            with patch('time.monotonic', side_effect=lambda: now[0]), patch('time.sleep', side_effect=pause), \
                 patch('subprocess.run', side_effect=service) as run, \
                 patch('subprocess.check_output', return_value='Result=success\n'), contextlib.redirect_stdout(output):
                exec(script, {})
            data = json.loads(output.getvalue())
            self.assertIsNone(data['before'])
            self.assertEqual(data['seeded'], '1 2 3 4 0 -900')
            self.assertEqual(data['seconds'], 420)
            self.assertEqual([call.args[0][-1] for call in run.call_args_list],
                             ['ascii-idle-reclaim.timer', 'ascii-idle-reclaim.service'])
            self.assertTrue(host.idle_reclaim_checks(data))
            self.assertFalse(host.idle_reclaim_checks(dict(data, after=None)))
            self.assertFalse(host.idle_reclaim_checks(dict(data, after='2 3 4 5 6 999')))

    def test_idle_reclaim_missing_state_times_out_without_false_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            now = [1000.0]
            def pause(seconds):
                now[0] += seconds
            output = io.StringIO()
            script = host.IDLE_RECLAIM.replace('/run/ascii-idle-reclaim', str(Path(tmp, 'absent')))
            with patch('time.monotonic', side_effect=lambda: now[0]), patch('time.sleep', side_effect=pause), \
                 patch('subprocess.run'), patch('subprocess.check_output', return_value='Result=success\n'), \
                 contextlib.redirect_stdout(output):
                exec(script, {})
            data = json.loads(output.getvalue())
            self.assertEqual(data['seconds'], 1020)
            self.assertIsNone(data['after'])
            self.assertFalse(host.idle_reclaim_checks(data))

    def test_cleanup_replay_requires_visible_workspace_and_withdrawn_admission(self):
        result = {'workspace_visible': True, 'users_mounts': [], 'endpoint_ready': False}
        self.assertTrue(host.cleanup_replay_checks(result))
        self.assertFalse(host.cleanup_replay_checks(dict(result, workspace_visible=False)))
        self.assertFalse(host.cleanup_replay_checks(dict(result, users_mounts=['overmount'])))
        self.assertFalse(host.cleanup_replay_checks(dict(result, endpoint_ready=True)))

    def test_storage_timeline_detects_deferred_cleanup_after_readiness(self):
        first = {'mounts': ['1 0 7:0 / /var/lib/motor-boat rw - xfs /dev/loop0 rw',
                            '2 0 7:0 /state/workspaces /Users rw - xfs /dev/loop0 rw'],
                 'devices': [1792, 1792], 'ready': True}
        self.assertTrue(host.storage_timeline_checks([first, first]))
        missing = dict(first, mounts=first['mounts'][:1], devices=[253, 1792])
        self.assertFalse(host.storage_timeline_checks([first, missing]))
        remounted = dict(first, mounts=[first['mounts'][0], first['mounts'][1].replace('2 0', '3 0')])
        self.assertFalse(host.storage_timeline_checks([first, remounted]))
        self.assertFalse(host.storage_timeline_checks([]))

    def test_client_requires_a_terminal(self):
        self.assertEqual(host.job_result('{"code":23,"terminal":{"outcome":{"kind":"exit","code":23}}}')["code"], 23)
        for text in ("{}", "not json", '{"code":true,"terminal":{}}'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                host.job_result(text)

    def fixture(self):
        return {"user": "motor-job", "uid": 997, "interfaces": ["lo"],
                "namespaces": {"net": "net:[2]", "pid": "pid:[1]", "mnt": "mnt:[2]"},
                "private_shm": True, "shm_writable": True, "nproc": "8",
                "execution_capabilities": {'checks': {'a_tmpdir_hardlink_exec': True, 'b_bin_sh': True, 'c_bun_shebang': True}},
                "limits": {"memory.max": "15569256448", "cpu.max": "800000 100000", "pids.max": "4096"},
                "provider_environment": {"ASCII_TOKEN": False, "BOAT_ID": False, "BOAT_CLI_TOKEN": False},
                "status_security": {'NoNewPrivs':'1','CapEff':'0000000000000000','CapBnd':'0000000000000000'},
                "proc_mountinfo": ['25 20 0:22 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw,hidepid=invisible',
                                   '26 25 0:22 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw,hidepid=invisible'],
                "proc_global": {'stat': {'readable': True, 'has_line': True}, 'meminfo': {'readable': True, 'has_line': True},
                                'sys_read_only': True, 'cmdline': {'readable': True, 'provider_names': False}},
                "cargo_target": {'env': '/run/motor/target', 'same_directory': True},
                "process_visibility": {'self_pid':10,'protected_pids':[1,2], 'proc':[{'pid':10,'uid':997,'same_cgroup':True}],
                    'ps':{'exit_code':0,'rows':[{'pid':10,'uid':997,'comm':'python3'}]},
                    'runner_signal_probe':{'pid':1,'allowed':False,'errno':1}},
                "provider_access": {'inherited_names': [], 'files': {'/home/user/.config/ascii/boat/config.json': {'readable':False}},
                                    'processes': {'1': {'readable':False}}}}

    def baseline(self):
        return {"namespaces": {"net": "net:[1]", "pid": "pid:[1]"},
                "memory_budget": {"mem_available_bytes": 16*(1<<30), "runner_idle_bytes": 1<<30,
                                  "margin_bytes": 512*(1<<20), "memory_max_bytes": 15569256448}}

    def test_real_job_limit_and_namespace_parser(self):
        job = self.fixture()
        baseline = self.baseline()
        self.assertTrue(all(host.isolation_checks(job, baseline).values()))
        for key, value in (("memory.max", "max"), ("cpu.max", "max 100000"), ("pids.max", "max")):
            job = self.fixture()
            job["limits"][key] = value
            self.assertFalse(all(host.isolation_checks(job, baseline).values()))
        job = self.fixture()
        job["interfaces"] = ["eth0", "lo"]
        self.assertFalse(host.isolation_checks(job, baseline)["loopback_only"])
        job = self.fixture()
        job["provider_environment"]["ASCII_TOKEN"] = True
        self.assertFalse(host.isolation_checks(job, baseline)["no_provider_job_environment"])

    def test_policy_uses_effective_sshd_values(self):
        sample = {"sshd": {"exit_code": 0, "stdout": "\n".join(k+" "+v for k, v in host.POLICY.items())}}
        self.assertTrue(all(host.policy_checks(sample).values()))
        sample["sshd"]["stdout"] = sample["sshd"]["stdout"].replace("permittty no", "permittty yes")
        self.assertFalse(host.policy_checks(sample)["permittty"])
        self.assertFalse(any(host.policy_checks({"sshd": {"stdout": ""}}).values()))

    def test_guest_sources_compile(self):
        compile(host.JOB, "job", "exec")
        compile(host.HOST, "host", "exec")
        self.assertIn("memory.max", host.JOB)
        for measured in ("/proc/stat", "/proc/meminfo", "/proc/sys", "/proc/cmdline", "/run/motor/target"):
            self.assertIn(measured, host.JOB)
        self.assertNotEqual(host.PORT, 9443)
        compile(host.SNAPSHOT_HYGIENE, 'snapshot-hygiene', 'exec')
        compile(host.STORAGE_STATE, 'storage-state', 'exec')

    def test_job_execution_probe_reports_hardlink_shell_and_bun_failures_separately(self):
        namespace = {}
        exec(host.EXEC_PROBE, namespace)
        probe = namespace['execution_capabilities']
        with tempfile.TemporaryDirectory() as directory:
            bun = Path(directory)/'bun'
            bun.write_text('#!/bin/sh\nprintf boat-bun-ok\n')
            bun.chmod(0o755)
            with patch.dict('os.environ', {'TMPDIR': directory}), patch('shutil.which', return_value=str(bun)):
                result = probe()
                self.assertEqual(result['failures'], [])
                self.assertTrue(result['a_tmpdir']['hardlink']['same_inode'])
                self.assertEqual(result['c_bun_shebang']['shebang'], '#!' + str(bun))
                self.assertEqual(set(result['mounts']), {'TMPDIR', '/tmp', 'bun'})
                for mount in result['mounts'].values():
                    self.assertEqual(mount['mount']['exit_code'], 0)
                    self.assertTrue(json.loads(mount['mount']['stdout'])['filesystems'][0]['options'])
                with patch('os.link', side_effect=OSError(errno.EOPNOTSUPP, 'hard links unsupported')):
                    result = probe()
                self.assertEqual(result['failures'], ['a_tmpdir_hardlink_exec'])
                self.assertEqual(result['a_tmpdir']['hardlink']['errno'], errno.EOPNOTSUPP)
                run = namespace['subprocess'].run
                def noexec(argv, **kwargs):
                    if argv[0].startswith(directory):
                        raise PermissionError(errno.EACCES, 'noexec mount')
                    return run(argv, **kwargs)
                with patch('subprocess.run', side_effect=noexec):
                    result = probe()
                self.assertEqual(result['failures'], ['a_tmpdir_hardlink_exec', 'c_bun_shebang'])
                def missing_shell(argv, **kwargs):
                    if argv[0] == '/bin/sh':
                        raise FileNotFoundError(errno.ENOENT, '/bin/sh missing')
                    return run(argv, **kwargs)
                with patch('subprocess.run', side_effect=missing_shell):
                    result = probe()
                self.assertEqual(result['failures'], ['b_bin_sh'])
            with patch.dict('os.environ', {'TMPDIR': directory}), patch('shutil.which', return_value='/' + 'x'*270):
                result = probe()
                self.assertEqual(result['failures'], ['c_bun_shebang'])
                self.assertGreater(result['c_bun_shebang']['shebang_bytes'], 256)

    def test_prepared_digest_tracks_source_and_signed_package_inputs_not_processes(self):
        inventory = {'packages':'pkg\t1\n', 'apt_release_files':{'noble':{'date':'fixed','sha256':'abc'}},
                     'cargo_seed': {'manifest_sha256': 'seed1'}, 'processes':[1]}
        digest = host.prepared_image_digest('rev1', inventory)
        self.assertEqual(len(digest), 64)
        self.assertEqual(digest, host.prepared_image_digest('rev1', dict(inventory, processes=[99])))
        self.assertNotEqual(digest, host.prepared_image_digest('rev2', inventory))
        self.assertNotEqual(digest, host.prepared_image_digest('rev1', dict(inventory, packages='pkg\t2\n')))
        self.assertNotEqual(digest, host.prepared_image_digest('rev1', dict(inventory, cargo_seed={'manifest_sha256': 'seed2'})))
        ready = host.snapshot_metadata({'name':'image','status':'ready','snapshotId':'artifact-id','sizeBytes':123})
        self.assertEqual((ready['name'],ready['id'],ready['size_bytes']), ('image','artifact-id',123))
        with self.assertRaises(ValueError):
            host.snapshot_metadata({'name':'image','status':'ready'})

    def test_snapshot_hygiene_removes_probe_authorization_and_identity_but_refuses_cli_config(self):
        namespace = {'__name__':'snapshot_test'}
        exec(host.SNAPSHOT_HYGIENE, namespace)
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            keys = home / '.ssh/authorized_keys'
            keys.parent.mkdir()
            keys.write_text('ssh-ed25519 tested probe\nssh-ed25519 witness keep\n')
            identity = home/'tenant-id'
            identity.write_text('probe')
            result = namespace['image_hygiene']([str(home)], 'tested', identity)
            self.assertTrue(result['probe_authorizations_absent'])
            self.assertEqual(keys.read_text(), 'ssh-ed25519 witness keep\n')
            self.assertFalse(identity.exists())
            config = home/'.config/ascii/boat/config.json'
            config.parent.mkdir(parents=True)
            config.write_text('{}')
            with self.assertRaisesRegex(RuntimeError, 'CLI config'):
                namespace['image_hygiene']([str(home)], 'tested', identity)

    def test_isolation_refusal_is_failure_not_an_empty_stdout_json_parse(self):
        self.assertEqual(host.isolation_result_checks({'code':200, 'stdout':''}, {}), {'isolation_job_completed':False})
        with self.assertRaisesRegex(ValueError, 'did not emit JSON evidence'):
            host.isolation_result_checks({'code':0, 'stdout':''}, {})

    def test_provider_access_requires_actual_checks_and_denies_token_exposure(self):
        access = self.fixture()['provider_access']
        self.assertTrue(host.provider_access_denied(access))
        access['processes']['1']['ascii_token_present'] = True
        self.assertFalse(host.provider_access_denied(access))
        self.assertFalse(host.provider_access_denied({'files':{}, 'processes':{}, 'inherited_names':[]}))

    def test_process_visibility_requires_own_cgroup_and_signal_permission_denial(self):
        job = self.fixture()
        self.assertTrue(all(host.process_visibility_checks(job).values()))
        job['process_visibility']['proc'].append({'pid':2,'uid':0,'same_cgroup':False})
        self.assertFalse(host.process_visibility_checks(job)['proc_only_job_processes'])
        job['process_visibility']['ps']['rows'].append({'pid':2,'uid':0,'comm':'motor-runner'})
        self.assertFalse(host.process_visibility_checks(job)['ps_only_job_processes'])
        job['process_visibility']['runner_signal_probe']['errno'] = 3
        self.assertFalse(host.process_visibility_checks(job)['cannot_signal_runner'])

    def test_job_reads_global_proc_entries_but_kernel_tunables_stay_read_only(self):
        job = self.fixture()
        self.assertTrue(all(host.proc_checks(job).values()))
        for entry, check in (('stat', 'proc_stat_has_btime'), ('meminfo', 'proc_meminfo_readable')):
            with self.subTest(entry=entry):
                job = self.fixture()
                job['proc_global'][entry] = {'readable': False, 'error': 'FileNotFoundError'}
                self.assertFalse(host.proc_checks(job)[check])
        job = self.fixture()
        job['proc_global']['stat']['has_line'] = False
        self.assertFalse(host.proc_checks(job)['proc_stat_has_btime'])
        job = self.fixture()
        job['proc_global']['sys_read_only'] = False
        self.assertFalse(host.proc_checks(job)['proc_sys_read_only'])
        job = self.fixture()
        job['proc_global']['cmdline']['provider_names'] = True
        self.assertFalse(host.proc_checks(job)['proc_cmdline_no_provider_names'])
        job = self.fixture()
        job['proc_mountinfo'][0] = job['proc_mountinfo'][0].replace('hidepid=invisible', 'hidepid=0')
        self.assertFalse(host.proc_checks(job)['proc_hides_other_users'])

    def test_pid_only_proc_mount_fails_the_parity_checks(self):
        # ProcSubset=pid mounts /proc with subset=pid, which hides /proc/stat and
        # /proc/meminfo. Boat jobs must see those entries as build-server jobs do.
        job = self.fixture()
        job['proc_mountinfo'] = [line + ',subset=pid' for line in job['proc_mountinfo']]
        job['proc_global'] = {'stat': {'readable': False, 'error': 'FileNotFoundError'},
                              'meminfo': {'readable': False, 'error': 'FileNotFoundError'},
                              'sys_read_only': False, 'sys_error': 'FileNotFoundError',
                              'cmdline': {'readable': False, 'error': 'FileNotFoundError'}}
        checks = host.proc_checks(job)
        for name in ('proc_hides_other_users', 'proc_stat_has_btime', 'proc_meminfo_readable'):
            self.assertFalse(checks[name], name)
        job = self.fixture()
        job['proc_mountinfo'] = [line + ',subset=pid' for line in job['proc_mountinfo']]
        self.assertFalse(host.proc_checks(job)['proc_hides_other_users'])
        self.assertNotIn('pid_only_proc_mount', host.isolation_checks(job, self.baseline()))

    def test_job_builds_in_the_fixed_target_folder(self):
        job = self.fixture()
        self.assertTrue(host.isolation_checks(job, self.baseline())['fixed_cargo_target_dir'])
        for target in ({'env': '/workspace/target', 'same_directory': False},
                       {'env': '/run/motor/target', 'same_directory': False},
                       {'env': None, 'same_directory': None, 'error': 'FileNotFoundError'}):
            with self.subTest(target=target):
                job['cargo_target'] = target
                self.assertFalse(host.isolation_checks(job, self.baseline())['fixed_cargo_target_dir'])

    def test_failure_diagnostics_capture_each_unit_even_when_status_is_nonzero(self):
        output = io.StringIO()
        def command(argv, **kwargs):
            return Mock(returncode=3 if 'status' in argv else 0, stdout='captured '+argv[-1], stderr='')
        with patch.object(host.subprocess, 'run', command), contextlib.redirect_stdout(output):
            exec(host.DIAGNOSTICS, {})
        data = json.loads(output.getvalue())
        self.assertEqual(set(data), {'motor-boat-restored.service','motor-boat-storage.service',r'var-lib-motor\x2dboat.mount','Users.mount','motor-boat.service','motor-boat-host.service','motor-boat-identity.service','motor-boat-bridge.service'})
        for unit, record in data.items():
            self.assertEqual(record['status']['exit_code'], 3)
            self.assertEqual(record['status']['argv'], ['/usr/bin/systemctl','status','--no-pager','-l',unit])
            self.assertEqual(record['journal']['argv'], ['/usr/bin/journalctl','-u',unit,'-b','--no-pager','-n','200'])

    def test_admission_parser_requires_real_auth_refusal_and_all_private_sockets(self):
        sample = {'endpoint_refusal': {'connected': True, 'frame': {'type':'lease_refused','reason':'auth'}},
                  'tenant_sockets': {path: {'uid':0, 'mode': '0o700' if path == '/run/motor-tenant' else '0o600', 'socket':path != '/run/motor-tenant'}
                     for path in ('/run/motor-tenant','/run/motor-tenant/jobs.sock','/run/motor-tenant/events.sock','/run/motor-tenant/admin.sock')}}
        self.assertTrue(all(host.admission_checks(sample).values()))
        sample['endpoint_refusal'] = {'error':'connection refused'}
        self.assertFalse(host.admission_checks(sample)['endpoint_rejects_invalid_token'])
        sample['tenant_sockets'] = {}
        self.assertFalse(host.admission_checks(sample)['root_private_tenant_sockets'])

    def endpoint_state(self, **overrides):
        state = {'units': {'motor-boat-identity.service': {'ActiveState': 'active'},
                           'motor-boat-restored.service': {'ActiveState': 'active'},
                           'motor-boat-host.service': {'ActiveState': 'active', 'MainPID': '42', 'NRestarts': '0'}},
                 'id_file': {'present': True, 'uid': 0, 'mode': '0o444', 'regular': True},
                 'marker': {'present': True, 'uid': 0, 'mode': '0o644', 'regular': True},
                 'listening_9443': True}
        state.update(overrides)
        return state

    def test_identity_install_replays_the_broker_byte_for_byte(self):
        self.assertEqual(host.IDENTITY_INSTALL, 'sudo install -D -m 0444 -o root -g root /tmp/ck-motor-tenant-id '
                         '/etc/motor/tenant/id && rm -f /tmp/ck-motor-tenant-id && cat /etc/motor/tenant/id')
        self.assertEqual(host.MARKER_READ, 'cat /run/motor/endpoint-ready')
        probe = Mock()
        probe.execute.return_value = {'exitCode': 0, 'stdout': 'bx_23456789', 'stderr': ''}
        record = host.RunnerHost.__new__(host.RunnerHost)
        record.probe = probe
        result = record.install_identity('bx_23456789')
        probe.request.assert_called_once_with('PUT', '/sandboxes/bx_23456789/files',
                                              {'path': '/tmp/ck-motor-tenant-id', 'content': 'bx_23456789'})
        probe.execute.assert_called_once_with('bx_23456789', host.IDENTITY_INSTALL)
        self.assertTrue(result['accepted'])
        # The broker rejects a trailing newline or any non-zero exit.
        for reply in ({'exitCode': 0, 'stdout': 'bx_23456789\n'}, {'exitCode': 1, 'stdout': 'bx_23456789'}):
            probe.execute.return_value = reply
            self.assertFalse(record.install_identity('bx_23456789')['accepted'])
        probe.request.side_effect = RuntimeError('file API refused')
        self.assertEqual(record.install_identity('bx_23456789')['error'], 'file API refused')

    def test_marker_must_be_the_exact_id_with_at_most_a_newline(self):
        for stdout, code, expected in (('bx_23456789', 0, True), ('bx_23456789\n', 0, True),
                                       ('bx_23456789\n\n', 0, False), ('bx_other', 0, False), ('bx_23456789', 1, False)):
            with self.subTest(stdout=stdout, code=code):
                self.assertEqual(host.marker_matches({'exit_code': code, 'stdout': stdout}, 'bx_23456789'), expected)

    def test_readiness_needs_bound_marker_listener_and_root_files(self):
        wait = {'ready': True, 'seconds': 12.0, 'marker': {'exit_code': 0, 'stdout': 'bx_23456789'},
                'state': self.endpoint_state()}
        self.assertTrue(all(host.readiness_checks(wait, 'bx_23456789').values()))
        slow = dict(wait, seconds=host.ENDPOINT_BOUND_S + 1)
        self.assertFalse(host.readiness_checks(slow, 'bx_23456789')['endpoint_ready_within_bound'])
        loose = dict(wait, state=self.endpoint_state(marker={'present': True, 'uid': 0, 'mode': '0o666', 'regular': True}))
        self.assertFalse(host.readiness_checks(loose, 'bx_23456789')['ready_marker_root_0644'])
        silent = dict(wait, state=self.endpoint_state(listening_9443=False))
        self.assertFalse(host.readiness_checks(silent, 'bx_23456789')['endpoint_listening'])
        self.assertFalse(any(host.readiness_checks({'ready': False}, 'bx_23456789').values()))

    def test_armed_state_requires_polling_watcher_and_no_endpoint_leftovers(self):
        absent = {'present': False}
        armed = self.endpoint_state(id_file=absent, marker=absent, listening_9443=False)
        self.assertTrue(host.armed_checks(armed))
        self.assertFalse(host.armed_checks(dict(armed, marker={'present': True})))
        self.assertFalse(host.armed_checks(dict(armed, listening_9443=True)))
        dead = json.loads(json.dumps(armed))
        dead['units']['motor-boat-identity.service']['ActiveState'] = 'inactive'
        self.assertFalse(host.armed_checks(dead))

    def test_storage_requires_disk_identity_and_verification_before_runner_not_unit_state(self):
        data = {'findmnt': {path:json.dumps({'filesystems':[{'target':path, 'fstype':'xfs'}]})
                            for path in ('/var/lib/motor-boat', '/Users')},
                'expected_backing':True, 'users_bind':True, 'boot_id':'boot',
                'prepared':{'boot_id':'boot','reflink':True,'verified_monotonic_ns':1000},
                'units':{name:{'ActiveState':'active','ExecMainStartTimestampMonotonic':'2'}
                         for name in ('motor-boat.service','motor-boat-host.service',r'var-lib-motor\x2dboat.mount','Users.mount')}}
        self.assertTrue(all(host.storage_checks(data).values()))
        for key in ('expected_backing', 'users_bind'):
            self.assertFalse(host.storage_checks(dict(data, **{key:False}))['reflink_disk_and_workspace_bind'])
        wrong = dict(data, findmnt={path:text.replace('xfs', 'ext4') for path, text in data['findmnt'].items()})
        self.assertFalse(host.storage_checks(wrong)['reflink_disk_and_workspace_bind'])
        stacked = dict(data, findmnt={path:json.dumps({'filesystems':[
            {'target':path,'fstype':'ext4'}, {'target':path,'fstype':'xfs'}]}) for path in data['findmnt']})
        self.assertFalse(host.storage_checks(stacked)['reflink_disk_and_workspace_bind'])
        self.assertFalse(host.storage_checks(dict(data, prepared={}))['storage_verified_before_services'])
        late = dict(data, prepared=dict(data['prepared'], verified_monotonic_ns=3000))
        self.assertFalse(host.storage_checks(late)['storage_verified_before_services'])

    def test_job_repository_lockfile_installs_regular_bytes_not_symlink_only_modules(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(host.subprocess, 'run'):
            repo = Path(tmp)
            host.job_repository(repo)
            manifest = json.loads((repo / 'package.json').read_text())
            self.assertEqual(manifest['dependencies'], {'boat-dep':'file:vendor/boat-dep.tgz'})
            lock = json.loads((repo / 'package-lock.json').read_text())
            package = lock['packages']['node_modules/boat-dep']
            self.assertEqual(package['resolved'], 'file:vendor/boat-dep.tgz')
            self.assertNotIn('link', package)
            payload = (repo / 'vendor/boat-dep.tgz').read_bytes()
            self.assertEqual(package['integrity'], 'sha512-' + base64.b64encode(hashlib.sha512(payload).digest()).decode())
            with tarfile.open(repo / 'vendor/boat-dep.tgz', 'r:gz') as tar:
                self.assertTrue(all(member.isfile() and member.size > 0 for member in tar.getmembers()))
                self.assertEqual(tar.extractfile('package/index.js').read(), b"module.exports = 'boat-node-modules';\n")

    def test_verify_prepared_hydrates_before_identity_phase_without_recipe_or_restart(self):
        probe = Mock()
        probe.data = {}
        probe.request.return_value = {'snapshot':{'name':'prepared','status':'ready','snapshotId':'id','sizeBytes':1}}
        probe.create.return_value = ({'id':'bx_test'}, {})
        runner = host.RunnerHost(probe)
        order = []
        probe.wait_hydrated.side_effect = lambda sid: order.append('hydrated')
        runner.phase = Mock(side_effect=lambda sid, label, *args, **kwargs: order.append(label))
        runner.replay_cleanup = Mock(side_effect=lambda sid: order.append('cleanup_replay'))
        runner.execute = Mock(side_effect=AssertionError('must not reconfigure guest'))
        with tempfile.TemporaryDirectory() as tmp, patch.object(host, 'ROOT', Path(tmp)), \
             patch.object(host, 'job_repository'), patch.dict(sys.modules, {'probe':Mock(READY={'running'})}):
            Path(tmp, '.cortexkit').mkdir()
            runner.verify_prepared('prepared', Path('.'), 'public', Path('client'))
        self.assertEqual(order, ['hydrated', 'prepared_clone', 'hydrated', 'resumed', 'cleanup_replay'])
        runner.execute.assert_not_called()
        probe.create.assert_called_once_with('prepared')


if __name__ == "__main__":
    unittest.main()
