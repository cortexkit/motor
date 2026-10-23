import json
import importlib.util
import contextlib
import io
import errno
from unittest.mock import patch, Mock
from pathlib import Path
import subprocess
import sys
import tempfile
import tarfile
import base64
import hashlib
import shlex
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
    def test_real_initial_job_checks_replay_without_relaxing_outcomes(self):
        live = json.loads(Path(__file__).with_name('fixtures').joinpath('pid-live-initial.json').read_text())
        lane = host.RunnerHost(Mock(data={}))
        for name, code, marker in [('node_modules_first', 0, 'boat-node-modules'), ('node_modules_reuse', 0, 'boat-node-modules'),
                                   ('exit', 23, 'boat-round-trip'), ('rust', 0, 'boat-hello'),
                                   ('sigkill_job', 137, None), ('pid1_ignores_job_sigkill', 0, 'boat-pid1-alive'),
                                   ('setsid_stop', 143, None)]:
            lane.check_job('initial.' + name, live['jobs'][name], code, marker=marker)
        lane.check_job('initial.job_stdin_eof', live['jobs']['job_stdin_eof'], 0, exact_stdout='boat-stdin-eof')
        lane.check_job('initial.setsid_kill', live['jobs']['setsid_kill'], 137, signal=9)
        checks = host.pid_checks(live['pid_job'], live['host'])
        self.assertTrue(all(checks.values()))
        for name in ('job_namespace_filter', 'job_clone3_pid_denied'):
            lane.check('initial.' + name, checks[name], live['pid_job'])
        self.assertEqual(len(lane.data['checks']), 11)
        self.assertTrue(all(check['status'] == 'PASS' for check in lane.data['checks']))

    def test_each_failed_runner_check_records_a_reason(self):
        lane = host.RunnerHost(Mock(data={}))
        lane.check('initial.required', False, {'observed':False})
        lane.check('initial.overridden', True, {}, status='FAIL')
        lane.check_job('initial.exit', {'code':23, 'terminal':{'outcome':{'kind':'exit', 'code':23}}}, 23)
        lane.check_job('initial.output', {'code':0, 'terminal':{'outcome':{'type':'exit', 'code':0}}}, 0, marker='boat-hello')
        self.assertTrue(all(row['status'] == 'FAIL' and row.get('reason') for row in lane.data['checks']))
        self.assertIn('terminal.outcome', lane.data['checks'][2]['reason'])
        self.assertIn("'type': 'exit'", lane.data['checks'][2]['reason'])
        self.assertIn("'kind': 'exit'", lane.data['checks'][2]['reason'])
        self.assertIn('boat-hello', lane.data['checks'][3]['reason'])

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

    def test_git_pushurl_job_passes_only_on_a_clean_single_url_report(self):
        passing = {'passed': True, 'observed': ['https://example.invalid/refused-push.git']}
        self.assertEqual((True, passing), host.git_pushurl_check({'code': 0, 'stdout': json.dumps(passing)}))
        # Ubuntu 24.04's git 2.43 keeps the inherited URL and the empty value.
        old = {'passed': False, 'observed': ['https://example.invalid/inherited-push.git', '',
                                             'https://example.invalid/refused-push.git']}
        self.assertEqual((False, old), host.git_pushurl_check({'code': 1, 'stdout': json.dumps(old)}))
        self.assertFalse(host.git_pushurl_check({'code': 0, 'stdout': json.dumps(dict(passing, passed=False))})[0])
        crashed = {'code': 1, 'stdout': ''}
        self.assertEqual((False, crashed), host.git_pushurl_check(crashed))
        phase = __import__('inspect').getsource(host.RunnerHost.phase)
        self.assertIn("('git_worktree_pushurl_reset', 'python3 -c ' + shlex.quote(GIT_PUSHURL_PROBE.read_text()))", phase)

    def test_wasm_opt_job_requires_version_120_and_a_completed_round_trip(self):
        report = {'passed': True, 'version': 'wasm-opt version 120 (version_120)', 'module_sha256': 'a' * 64,
                  'round_trip_sha256': 'b' * 64, 'round_trip_bytes': 8}
        self.assertEqual((True, report), host.wasm_opt_check({'code': 0, 'stdout': json.dumps(report)}))
        older = dict(report, passed=False, version='wasm-opt version 116')
        self.assertFalse(host.wasm_opt_check({'code': 1, 'stdout': json.dumps(older)})[0])
        self.assertFalse(host.wasm_opt_check({'code': 0, 'stdout': json.dumps(dict(report, version='wasm-opt version 116'))})[0])
        invalid = dict(report, round_trip_sha256='not-a-digest')
        self.assertFalse(host.wasm_opt_check({'code': 0, 'stdout': json.dumps(invalid)})[0])
        phase = __import__('inspect').getsource(host.RunnerHost.phase)
        self.assertIn("('wasm_opt_120_round_trip', 'python3 -c ' + shlex.quote(WASM_OPT_PROBE.read_text()))", phase)
        self.assertIn("label + '.' + name, *wasm_opt_check(parsed)", phase)

    def test_parity_checker_remeasures_live_files_and_uses_operator_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp, 'wasm32-unknown-unknown')
            target.mkdir()
            checker = Path(tmp, 'checker.py')
            checker.write_text('from pathlib import Path\ndef check(manifest, role, job, gnu_test_sha256=None, cargo_registry=None):\n'
                               f'    return {{"passed": Path({str(target)!r}).is_dir(), "job": job, "syscalls": len(syscall_surface.expectations(job))}}\n')
            with patch.object(host, 'PARITY_SCRIPT', checker):
                source = host.parity_source(job=True)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
            self.assertTrue(json.loads(output.getvalue())['passed'])
            self.assertEqual(30, json.loads(output.getvalue())['syscalls'])
            target.rmdir()
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
            self.assertFalse(json.loads(output.getvalue())['passed'])
            phase = __import__('inspect').getsource(host.RunnerHost.phase)
            self.assertIn("label + '.toolchain_parity'", phase)
            self.assertIn('("job_toolchain_parity", "cargo --version', phase)

    def test_root_owned_path_check_runs_the_operator_rule_on_boat_roots(self):
        with tempfile.TemporaryDirectory() as tmp:
            checker = Path(tmp, 'root_paths.py')
            checker.write_text('BOAT_ROOTS = ("/fixture",)\n'
                               'BOAT_PROVIDER_RESTORED = {"/fixture/tool": 1000}\n'
                               'def check(roots, provider_files=None):\n'
                               '    return {"roots": list(roots), "entries": 7, "offender_count": 0, "offenders": [],\n'
                               '            "provider_files": provider_files}\n')
            with patch.object(host, 'ROOT_PATHS_SCRIPT', checker):
                source = host.root_paths_source()
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
        report = json.loads(output.getvalue())
        self.assertEqual(report['roots'], ['/fixture'])
        # The live check passes exactly the provider-restored allowance.
        self.assertEqual(report['provider_files'], {'/fixture/tool': 1000})
        self.assertTrue(host.root_paths_passed(report))
        self.assertIn('BOAT_ROOTS', host.ROOT_PATHS_SCRIPT.read_text())
        for bad in (dict(report, offender_count=1, offenders=['/usr/local/bin: uid=1001 gid=1001']),
                    dict(report, entries=0), {}):
            self.assertFalse(host.root_paths_passed(bad))
        phase = __import__('inspect').getsource(host.RunnerHost.phase)
        self.assertIn("label + '.root_owned_paths'", phase)

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
        result = {'workspace_visible': True, 'users_mounts': [], 'endpoint_ready': False,
                  'endpoint_ready_before': True, 'withdrawal_latency_seconds': 1.1,
                  'endpoint_ready_reappeared': False, 'post_withdrawal_observation_seconds': 10}
        self.assertTrue(host.cleanup_replay_checks(result))
        self.assertFalse(host.cleanup_replay_checks(dict(result, workspace_visible=False)))
        self.assertFalse(host.cleanup_replay_checks(dict(result, users_mounts=['overmount'])))
        self.assertFalse(host.cleanup_replay_checks(dict(result, endpoint_ready=True)))
        self.assertFalse(host.cleanup_replay_checks(dict(result, endpoint_ready_before=False)))
        self.assertFalse(host.cleanup_replay_checks(dict(result, endpoint_ready_reappeared=True)))
        self.assertFalse(host.cleanup_replay_checks(dict(result, post_withdrawal_observation_seconds=9.9)))
        self.assertTrue(host.cleanup_replay_checks(dict(result, withdrawal_latency_seconds=3)))
        for latency in ('never', None, True, -1, 3.001, float('nan'), float('inf')):
            with self.subTest(latency=latency):
                self.assertFalse(host.cleanup_replay_checks(dict(result, withdrawal_latency_seconds=latency)))

    def cleanup_replay(self, withdraw_after, activation_seconds=0, diagnostics_timeout=False, activation_timeout=False,
                       reappear_after=None):
        with tempfile.TemporaryDirectory() as tmp:
            users, ready, mountinfo = (Path(tmp, name) for name in ('Users', 'ready', 'mountinfo'))
            users.mkdir()
            ready.touch()
            mountinfo.write_text('')
            now, pauses, commands = [100.0], [], []
            did_reappear = [False]
            properties = Path(__file__).with_name('fixtures').joinpath('storage-watch-systemctl-show.txt').read_text()
            def command(argv, **kwargs):
                commands.append(argv)
                if argv[0] == 'umount':
                    self.assertEqual(argv, ['umount', '--', str(users)])
                    return Mock(returncode=0)
                if diagnostics_timeout:
                    raise __import__('subprocess').TimeoutExpired(argv, 10)
                return Mock(returncode=0, stdout=properties if argv[0] == 'systemctl' else
                            '[ 12.000000] Boat storage unhealthy; withdrawing endpoint readiness: view lost\n', stderr='')
            def pause(seconds):
                pauses.append(seconds)
                now[0] = round(now[0] + seconds, 9)
                elapsed = round(now[0] - 100, 9)
                if withdraw_after is not None and elapsed >= withdraw_after:
                    ready.unlink(missing_ok=True)
                    if reappear_after is not None and not did_reappear[0] and elapsed >= withdraw_after + reappear_after:
                        ready.touch()
                        did_reappear[0] = True
            def communicate(timeout=None):
                if activation_timeout and timeout is not None:
                    raise __import__('subprocess').TimeoutExpired('systemctl', timeout)
                now[0] += activation_seconds
                return '', ''
            activation = Mock(returncode=0)
            activation.communicate.side_effect = communicate
            script = host.DEFERRED_CLEANUP_REPLAY.replace('/Users', str(users)).replace(
                '/run/motor/endpoint-ready', str(ready)).replace('/proc/self/mountinfo', str(mountinfo))
            output = io.StringIO()
            with patch('time.monotonic', side_effect=lambda: now[0]), patch('time.sleep', side_effect=pause), \
                 patch('subprocess.run', side_effect=command), patch('subprocess.Popen', return_value=activation) as popen, \
                 contextlib.redirect_stdout(output):
                exec(script, {})
            self.assertEqual(popen.call_args.args[0], ['systemctl', 'start', 'Users.mount'])
            return json.loads(output.getvalue()), commands, pauses, activation

    def test_cleanup_replay_polls_during_activation_and_captures_watcher_diagnostics(self):
        data, commands, pauses, _ = self.cleanup_replay(.7, activation_seconds=4)
        self.assertTrue(host.cleanup_replay_checks(data))
        self.assertAlmostEqual(data['withdrawal_latency_seconds'], .7)
        self.assertTrue(pauses)
        self.assertTrue(all(0 < seconds <= .1 for seconds in pauses))
        self.assertAlmostEqual(data['post_withdrawal_observation_seconds'], 10)
        self.assertFalse(data['endpoint_ready_reappeared'])
        self.assertEqual(data['activation_code'], 0)
        self.assertFalse(data['activation_timed_out'])
        for phase in ('before', 'after'):
            state = data['storage_watch_' + phase]
            self.assertEqual(state['returncode'], 0)
            self.assertEqual(state['properties'], {'ActiveState': 'active', 'SubState': 'running',
                'NRestarts': '0', 'ExecMainStartTimestamp': 'Wed 2026-10-21 12:55:08 UTC'})
            self.assertEqual(state['stdout'], 'NRestarts=0\nExecMainStartTimestamp=Wed 2026-10-21 12:55:08 UTC\nActiveState=active\nSubState=running\n')
        self.assertEqual(commands[-1], ['journalctl', '--boot=0', '--unit=motor-boat-storage-watch.service',
                                       '--no-pager', '--output=short-monotonic'])
        self.assertIn('Boat storage unhealthy', data['storage_watch_journal']['stdout'])
        self.assertEqual([command[0] for command in commands], ['systemctl', 'umount', 'systemctl', 'journalctl'])

    def test_cleanup_replay_rejects_transient_republication_after_withdrawal(self):
        data, _, pauses, _ = self.cleanup_replay(.5, reappear_after=.3)
        self.assertFalse(data['endpoint_ready'])
        self.assertTrue(data['endpoint_ready_reappeared'])
        self.assertAlmostEqual(data['reappearance_latency_seconds'], .3)
        self.assertAlmostEqual(data['post_withdrawal_observation_seconds'], 10)
        self.assertAlmostEqual(sum(pauses), 10.5)
        self.assertFalse(host.cleanup_replay_checks(data))

    def test_cleanup_replay_records_late_withdrawal_without_relaxing_deadline(self):
        data, _, _, _ = self.cleanup_replay(4.2)
        self.assertFalse(data['endpoint_ready'])
        self.assertAlmostEqual(data['withdrawal_latency_seconds'], 4.2)
        self.assertFalse(host.cleanup_replay_checks(data))

    def test_cleanup_replay_records_never_after_ten_seconds(self):
        data, _, pauses, _ = self.cleanup_replay(None)
        self.assertEqual(data['withdrawal_latency_seconds'], 'never')
        self.assertAlmostEqual(sum(pauses), 10)
        self.assertTrue(all(0 < seconds <= .1 for seconds in pauses))
        self.assertTrue(data['endpoint_ready'])
        self.assertFalse(host.cleanup_replay_checks(data))

    def test_cleanup_replay_keeps_observations_when_diagnostics_time_out(self):
        data, _, _, _ = self.cleanup_replay(.5, diagnostics_timeout=True)
        self.assertTrue(host.cleanup_replay_checks(data))
        for name in ('storage_watch_before', 'storage_watch_after', 'storage_watch_journal'):
            self.assertIsNone(data[name]['returncode'])
            self.assertIn('timed out', data[name]['stderr'])

    def test_cleanup_replay_reaps_timed_out_mount_activation(self):
        data, _, _, activation = self.cleanup_replay(.5, activation_timeout=True)
        self.assertTrue(data['activation_timed_out'])
        activation.kill.assert_called_once_with()
        self.assertEqual(activation.communicate.call_count, 2)
        self.assertTrue(host.cleanup_replay_checks(data))

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
        self.assertEqual(host.job_result('{"code":23,"terminal":{"outcome":{"type":"exit","code":23}}}')["code"], 23)
        for text in ("{}", "not json", '{"code":true,"terminal":{}}'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                host.job_result(text)

    def fixture(self):
        return {"user": "motor-job", "uid": 997, "interfaces": ["lo"],
                "pid_namespace": json.loads(Path(__file__).with_name('fixtures').joinpath('pid-namespace.json').read_text())['pid_namespace'],
                "namespaces": {"net": "net:[2]", "pid": "pid:[2]", "mnt": "mnt:[2]"},
                "private_shm": True, "shm_writable": True, "nproc": "8",
                "execution_capabilities": {'checks': {'a_tmpdir_hardlink_exec': True, 'b_bin_sh': True, 'c_bun_shebang': True}},
                "limits": {"memory.max": "15569256448", "cpu.max": "800000 100000", "pids.max": "4096"},
                "provider_environment": {"ASCII_TOKEN": False, "BOAT_ID": False, "BOAT_CLI_TOKEN": False},
                "status_security": {'NoNewPrivs':'1','CapEff':'0000000000000000','CapBnd':'0000000000000000'},
                "proc_mountinfo": ['25 20 0:22 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw,hidepid=invisible',
                                   '26 25 0:22 /sys /proc/sys ro,nosuid,nodev,noexec,relatime - proc proc rw,hidepid=invisible'],
                "proc_global": {'stat': {'readable': True, 'has_line': True}, 'meminfo': {'readable': True, 'has_line': True},
                                'locks': {'readable': True, 'has_line': True}, 'net_unix': {'readable': True, 'has_line': True},
                                'sys_read_only': True, 'cmdline': {'readable': True, 'provider_names': False}},
                "cargo_target": {'env': '/run/motor/target', 'same_directory': True},
                "process_visibility": {'self_pid':2,'protected_pids':[1,101,102], 'proc':[{'pid':1,'uid':997,'same_cgroup':True}, {'pid':2,'uid':997,'same_cgroup':True}],
                    'ps':{'exit_code':0,'rows':[{'pid':1,'uid':997,'comm':'bash'}, {'pid':2,'uid':997,'comm':'python3'}]},
                    'runner_signal_probe':json.loads(Path(__file__).with_name('fixtures').joinpath('pid-namespace.json').read_text())['pid_namespace']['signals'][1]},
                "provider_access": {'inherited_names': [], 'files': {'/home/user/.config/ascii/boat/config.json': {'readable':False}},
                                    'processes': {'1': {'readable':False}}}}

    def baseline(self):
        return {"namespaces": {"net": "net:[1]", "pid": "pid:[1]"}, "systemd_version":255,
                "job_security_reference": json.loads(Path(__file__).with_name('fixtures').joinpath('pid-namespace.json').read_text())['reference'],
                "sizing": {"nproc": 8},
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

    def test_cpu_limit_follows_the_measured_sandbox_and_requested_vcpus(self):
        default = dict(self.baseline(), sizing={"nproc": 4})
        job = self.fixture()
        job["nproc"], job["limits"]["cpu.max"] = "4", "400000 100000"
        self.assertTrue(host.isolation_checks(job, default)["cpu_limit"])
        job["limits"]["cpu.max"] = "200000 100000"
        self.assertTrue(host.isolation_checks(job, default, 2)["cpu_limit"])
        # A job sized for large on a default sandbox, or a host CPU count never measured.
        large_job = self.fixture()
        self.assertFalse(host.isolation_checks(large_job, default)["cpu_limit"])
        job["limits"]["cpu.max"] = "400000 100000"
        self.assertFalse(host.isolation_checks(job, dict(self.baseline(), sizing={}))["cpu_limit"])

    def sized_host(self, nproc, max_weight, ceiling, memory_max=6979321856, flag=6979321856):
        return {"sizing": {"nproc": nproc, "affinity_cpus": nproc,
                           "runner_argv": ["/usr/local/bin/motor-runner", "serve", "--max-weight", str(max_weight),
                                           "--rustc-slots", str(max_weight), "--boat-memory-max-bytes", str(flag)],
                           "endpoint_config_path": "/run/motor/sandbox.json", "endpoint_config": {"vcpu_ceiling": ceiling},
                           "state_disk": {"total_bytes": 50 << 30, "free_bytes": 30 << 30}},
                "memory_budget": {"mem_available_bytes": 7612203008, "runner_idle_bytes": 120 << 20,
                                  "margin_bytes": 512 << 20, "memory_max_bytes": memory_max},
                "meminfo_at_idle": "MemTotal:        8127932 kB\nMemFree:  100 kB\nMemAvailable:    7300000 kB\n"}

    def test_sizing_record_reads_what_the_sandbox_measured(self):
        record = host.sizing_record(self.sized_host(4, 4, 4), 4)
        self.assertEqual({key: record[key] for key in ("usable_vcpus", "runner_max_weight", "runner_rustc_slots",
                                                       "endpoint_vcpu_ceiling", "job_vcpus", "runner_memory_max_bytes",
                                                       "runner_memory_flag_bytes", "mem_total_bytes",
                                                       "mem_available_before_runner_bytes", "mem_available_at_idle_bytes")},
                         {"usable_vcpus": 4, "runner_max_weight": 4, "runner_rustc_slots": 4, "endpoint_vcpu_ceiling": 4,
                          "job_vcpus": 4, "runner_memory_max_bytes": 6979321856, "runner_memory_flag_bytes": 6979321856,
                          "mem_total_bytes": 8127932 * 1024, "mem_available_before_runner_bytes": 7612203008,
                          "mem_available_at_idle_bytes": 7300000 * 1024})
        self.assertTrue(all(host.sizing_checks(record).values()))

    def test_runner_sized_for_the_build_sandbox_fails_on_a_smaller_one(self):
        # An image built on large still starting the runner or endpoint with 8 on a 4-vCPU default.
        for runner, ceiling in ((8, 4), (4, 8), (8, 8)):
            with self.subTest(runner=runner, ceiling=ceiling):
                record = host.sizing_record(self.sized_host(4, runner, ceiling), 4)
                self.assertFalse(host.sizing_checks(record)["runner_vcpus_follow_sandbox"])
                self.assertIn("sandbox has 4 usable vCPUs", host.sizing_reason("runner_vcpus_follow_sandbox", record))
        record = host.sizing_record(self.sized_host(4, 4, 4, flag=15569256448), 4)
        self.assertFalse(host.sizing_checks(record)["runner_memory_from_measurement"])
        record = host.sizing_record(self.sized_host(4, 4, 4, memory_max=16 << 30, flag=16 << 30), 4)
        self.assertFalse(host.sizing_checks(record)["runner_memory_from_measurement"])

    def test_job_asking_for_more_vcpus_than_the_size_has_is_skipped(self):
        self.assertEqual(host.job_vcpus(None, 4, "default"), (4, None))
        self.assertEqual(host.job_vcpus(2, 4, "default"), (2, None))
        weight, reason = host.job_vcpus(8, 4, "default")
        self.assertIsNone(weight)
        self.assertEqual(reason, "skipped: the job asks for 8 vCPUs and this default sandbox has 4 usable vCPUs")
        self.assertEqual(host.JOB_VCPUS, {})

    def test_verify_prepared_refuses_a_sandbox_of_the_wrong_size(self):
        probe = Mock()
        probe.data = {}
        probe.size = 'default'
        probe.request.return_value = {'snapshot':{'name':'prepared','status':'ready','snapshotId':'id','sizeBytes':1}}
        probe.create.return_value = ({'id':'bx_test', 'vcpu':8, 'memoryGB':16}, {})
        runner = host.RunnerHost(probe)
        with self.assertRaisesRegex(ValueError, 'default sizing'):
            runner.verify_prepared('prepared', Path('.'), 'public', Path('client'))

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

    def test_process_visibility_requires_own_cgroup_and_outside_pid_esrch(self):
        job = self.fixture()
        self.assertTrue(all(host.process_visibility_checks(job).values()))
        job['process_visibility']['proc'].append({'pid':2,'uid':0,'same_cgroup':False})
        self.assertFalse(host.process_visibility_checks(job)['proc_only_job_processes'])
        job['process_visibility']['ps']['rows'].append({'pid':2,'uid':0,'comm':'motor-runner'})
        self.assertFalse(host.process_visibility_checks(job)['ps_only_job_processes'])
        job['process_visibility']['runner_signal_probe']['errno'] = errno.EPERM
        self.assertFalse(host.process_visibility_checks(job)['cannot_signal_runner'])

    def test_job_reads_global_proc_entries_but_kernel_tunables_stay_read_only(self):
        job = self.fixture()
        self.assertTrue(all(host.proc_checks(job).values()))
        for entry, check in (('stat', 'proc_stat_has_btime'), ('meminfo', 'proc_meminfo_readable'),
                             ('locks', 'proc_locks_readable'), ('net_unix', 'proc_net_unix_readable')):
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
                              'locks': {'readable': False, 'error': 'FileNotFoundError'},
                              'net_unix': {'readable': False, 'error': 'FileNotFoundError'},
                              'sys_read_only': False, 'sys_error': 'FileNotFoundError',
                              'cmdline': {'readable': False, 'error': 'FileNotFoundError'}}
        checks = host.proc_checks(job)
        for name in ('proc_hides_other_users', 'proc_stat_has_btime', 'proc_meminfo_readable',
                     'proc_locks_readable', 'proc_net_unix_readable'):
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

    def hang_fixture(self, tmp):
        """A /proc, cgroup and runner state tree: runner 100 with two threads,
        a job process 200 in a motor-job cgroup, and one recent job record."""
        proc, cgroup, state = (Path(tmp, name) for name in ('proc', 'cgroup', 'state'))
        for pid, comm, threads in ((100, 'motor-runner', (100, 101)), (200, 'npm ci', (200,)), (300, 'sshd', (300,)),
                                   (150, 'git', (150,))):
            Path(proc, str(pid)).mkdir(parents=True)
            Path(proc, str(pid), 'comm').write_text(comm + '\n')
            Path(proc, str(pid), 'cmdline').write_text(comm.replace(' ', '\0') + '\0')
            Path(proc, str(pid), 'status').write_text(f'Name:\t{comm}\nThreads:\t{len(threads)}\n')
            for tid in threads:
                task = Path(proc, str(pid), 'task', str(tid))
                task.mkdir(parents=True)
                task.joinpath('comm').write_text(f'{comm}-{tid}\n')
                task.joinpath('wchan').write_text('futex_wait_queue')
                task.joinpath('stat').write_text(f'{tid} ({comm} x) S 1 2 3')
                task.joinpath('syscall').write_text('202 0x1 0x2')
                task.joinpath('stack').write_text('[<0>] futex_wait_queue+0x5c/0x90\n')
        for name in ('loadavg', 'meminfo'):
            Path(proc, name).write_text(name + ' fixture\n')
        unit = Path(cgroup, 'system.slice', 'motor-job-0190.service', 'job')
        unit.mkdir(parents=True)
        unit.parent.joinpath('cgroup.procs').write_text('')
        unit.joinpath('cgroup.procs').write_text('200\n')
        Path(cgroup, 'system.slice', 'sshd.service').mkdir()
        Path(cgroup, 'system.slice', 'sshd.service', 'cgroup.procs').write_text('300\n')
        Path(cgroup, 'system.slice', 'motor-boat.service').mkdir()
        Path(cgroup, 'system.slice', 'motor-boat.service', 'cgroup.procs').write_text('100\n150\n')
        job = '0190aaaa-0000-7000-8000-000000000001'
        Path(state, 'jobs', job).mkdir(parents=True)
        Path(state, 'jobs', job, 'record.json').write_text(json.dumps(
            {'id': job, 'state': 'queued', 'accepted_ms': 5, 'started_ms': None, 'terminal': None,
             'request': {'command': 'node -e 1', 'env': {'SECRET_NAME': 'value'}}}))
        Path(state, 'sandboxes', job, 'output').mkdir(parents=True)
        Path(state, 'sandboxes', job, 'output', 'stderr').write_text('x' * 9000 + 'npm warn tail')
        Path(state, 'tenant-events.json').write_text('{"setup": "prepare javascript dependencies"}')
        return proc, cgroup, state, job

    def run_hang_diagnostics(self, tmp, which=lambda name: None):
        proc, cgroup, state, job = self.hang_fixture(tmp)
        commands = []
        def command(argv, **kwargs):
            commands.append(argv)
            if argv[:2] == ['systemctl', 'show']:
                return Mock(returncode=0, stdout='MainPID=100\nActiveState=active\n' if argv[2] == 'motor-boat.service'
                            else 'MainPID=0\n', stderr='')
            if argv[0] == 'ps':
                return Mock(returncode=0, stderr='', stdout='  PID   TID\n  100   101 motor-runner\n  150   150 git\n  200   200 npm ci\n'
                            '  300   300 sshd\n  400   400 python3 /usr/local/libexec/motor-boat-bridge.py\n')
            return Mock(returncode=0, stdout='captured ' + argv[0], stderr='')
        script = (host.JOB_HANG_DIAGNOSTICS.replace("PROC = '/proc'", f'PROC = {str(proc)!r}')
                  .replace("CGROUP = '/sys/fs/cgroup'", f'CGROUP = {str(cgroup)!r}')
                  .replace("STATE = '/var/lib/motor-boat/state/boat'", f'STATE = {str(state)!r}'))
        self.assertNotEqual(script, host.JOB_HANG_DIAGNOSTICS)
        output = io.StringIO()
        with patch('subprocess.run', side_effect=command), patch('shutil.which', side_effect=which), \
             contextlib.redirect_stdout(output):
            exec(script, {})
        return json.loads(output.getvalue()), commands, job

    def test_hang_diagnostics_capture_runner_threads_job_processes_and_recent_records(self):
        with tempfile.TemporaryDirectory() as tmp:
            data, commands, job = self.run_hang_diagnostics(tmp)
        journal = data['journal']['argv']
        self.assertEqual(journal[:6], ['journalctl', '--no-pager', '-o', 'short-iso-precise', '--since', '-5min'])
        for unit in ('motor-boat.service', 'motor-boat-host.service', 'motor-boat-bridge.service', 'motor-job-*'):
            self.assertIn(unit, journal)
        # The runner by its unit's MainPID; its setup child and the job
        # process by their cgroups; an unrelated service process is left out.
        self.assertEqual(set(data['processes']), {'100', '150', '200'})
        self.assertEqual(data['cgroup_pids'], [100, 150, 200])
        self.assertEqual(data['tenant_events']['tail'], '{"setup": "prepare javascript dependencies"}')
        runner = data['processes']['100']
        self.assertEqual(set(runner['tasks']), {'100', '101'})
        self.assertEqual(runner['tasks']['101']['wchan'], 'futex_wait_queue')
        self.assertEqual(runner['tasks']['101']['state'], 'S')
        self.assertIn('futex_wait_queue', runner['tasks']['101']['stack'])
        self.assertEqual(runner['user_stacks'], {'error': 'neither eu-stack nor gdb is installed'})
        self.assertNotIn('user_stacks', data['processes']['200'])
        self.assertEqual(data['processes']['200']['cmdline'], 'npm ci ')
        self.assertEqual(data['ps']['stdout'].splitlines(), ['  PID   TID', '  100   101 motor-runner', '  150   150 git', '  200   200 npm ci',
                                                             '  400   400 python3 /usr/local/libexec/motor-boat-bridge.py'])
        recent = data['recent_jobs'][job]
        self.assertEqual((recent['state'], recent['started_ms'], recent['command']), ('queued', None, 'node -e 1'))
        self.assertNotIn('request', recent)
        self.assertEqual(recent['output']['stderr']['size'], 9013)
        self.assertTrue(recent['output']['stderr']['tail'].endswith('npm warn tail'))
        self.assertIn('error', recent['output']['stdout'])
        self.assertIn(['df', '-B1', '/var/lib/motor-boat', '/'], commands)

    def test_hang_diagnostics_take_user_space_stacks_with_the_installed_dumper(self):
        with tempfile.TemporaryDirectory() as tmp:
            data, commands, _ = self.run_hang_diagnostics(tmp, which=lambda name: '/usr/bin/gdb' if name == 'gdb' else None)
        gdb = ['gdb', '-p', '100', '-batch', '-nx', '-ex', 'set pagination off', '-ex', 'thread apply all bt']
        self.assertEqual(data['processes']['100']['user_stacks']['argv'], gdb)
        self.assertEqual([argv for argv in commands if argv[0] in ('gdb', 'eu-stack')], [gdb])
        with tempfile.TemporaryDirectory() as tmp:
            data, _, _ = self.run_hang_diagnostics(tmp, which=lambda name: '/usr/bin/' + name)
        self.assertEqual(data['processes']['100']['user_stacks']['argv'], ['eu-stack', '-p', '100'])

    def test_client_failure_records_sandbox_diagnostics_without_masking_the_failure(self):
        probe = Mock(data={})
        probe.execute.return_value = {'exitCode': 0, 'stdout': '{"journal": {"stdout": "runner log"}}', 'stderr': ''}
        lane = host.RunnerHost(probe)
        result = {'argv': ['client'], 'error': 'timed out after 150 seconds'}
        lane.client_failure_diagnostics('bx_1', result)
        self.assertEqual(result['sandbox_diagnostics'], {'journal': {'stdout': 'runner log'}})
        self.assertIn(shlex.quote(host.JOB_HANG_DIAGNOSTICS), probe.execute.call_args.args[1])
        self.assertTrue(probe.execute.call_args.args[1].startswith('sudo -n /usr/bin/python3 -c '))
        probe.execute.side_effect = ValueError('timed out polling command 7')
        failed = {'argv': ['client']}
        lane.client_failure_diagnostics('bx_1', failed)
        self.assertEqual(failed['sandbox_diagnostics_error'], 'timed out polling command 7')
        phase = __import__('inspect').getsource(host.RunnerHost.phase)
        failure = phase.index('failure = "motor client failed: " + json.dumps(result)')
        collect = phase.index('self.client_failure_diagnostics(sid, record[name])')
        self.assertLess(failure, collect)
        self.assertLess(collect, phase.index('raise ValueError(failure', collect))

    def test_timed_out_client_keeps_what_it_printed(self):
        script = 'import sys, time; print("accepted", file=sys.stderr, flush=True); time.sleep(30)'
        result = host.process([sys.executable, '-c', script], timeout=2)
        self.assertIn('timed out after 2 seconds', result['error'])
        self.assertIn('accepted', result['partial_stderr'])
        self.assertNotIn('exit_code', result)

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
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            with patch.object(host.subprocess, 'run'):
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
            # Bun and pnpm lockfiles too, so a setup tool the runner cannot
            # find in the Boat image fails the node_modules jobs.
            for manager, lockfile in (('bun', 'bun.lock'), ('pnpm', 'pnpm-lock.yaml')):
                self.assertTrue((repo / manager / lockfile).is_file())
                self.assertFalse((repo / manager / 'node_modules').exists())
            command = host.NODE_CHECK.split('; node -e')[0] + '; echo installed'
            missing = subprocess.run(['sh', '-c', command], cwd=repo, capture_output=True, text=True)
            self.assertEqual('runner setup did not install the bun fixture\n', missing.stderr)
            for manager in host.JAVASCRIPT_FIXTURE_MANAGERS:
                (repo / manager / 'node_modules/fixture-dep').mkdir(parents=True)
                (repo / manager / 'node_modules/fixture-dep/index.js').write_text('')
            self.assertEqual('installed\n', subprocess.run(['sh', '-c', command], cwd=repo,
                                                           capture_output=True, text=True).stdout)

    def test_verify_prepared_hydrates_before_identity_phase_without_recipe_or_restart(self):
        probe = Mock()
        probe.data = {}
        probe.size = 'default'
        probe.request.return_value = {'snapshot':{'name':'prepared','status':'ready','snapshotId':'id','sizeBytes':1}}
        probe.create.return_value = ({'id':'bx_test', 'vcpu':4, 'memoryGB':8}, {})
        runner = host.RunnerHost(probe)
        order = []
        probe.wait_hydrated.side_effect = lambda sid: order.append('hydrated')
        def phase(sid, label, *args, **kwargs):
            order.append(label)
            return {'node_modules_first': {'result': {'code': 0, 'stdout': 'boat-node-modules'}}}
        runner.phase = Mock(side_effect=phase)
        def disk_quota(*args):
            order.append('disk_quota')
            runner.data['disk_quota'] = {}
        runner.disk_quota = Mock(side_effect=disk_quota)
        runner.replay_cleanup = Mock(side_effect=lambda sid: order.append('cleanup_replay'))
        runner.execute = Mock(side_effect=AssertionError('must not reconfigure guest'))
        with tempfile.TemporaryDirectory() as tmp, patch.object(host, 'ROOT', Path(tmp)), \
             patch.object(host, 'job_repository'), patch.dict(sys.modules, {'probe':Mock(READY={'running'})}):
            Path(tmp, '.cortexkit').mkdir()
            runner.verify_prepared('prepared', Path('.'), 'public', Path('client'))
        self.assertEqual(order, ['hydrated', 'prepared_clone', 'disk_quota', 'hydrated', 'resumed', 'cleanup_replay'])
        runner.execute.assert_not_called()
        probe.create.assert_called_once_with('prepared')


if __name__ == "__main__":
    unittest.main()
