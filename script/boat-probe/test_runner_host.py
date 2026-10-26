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
import re
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
        phase = __import__('inspect').getsource(host.RunnerHost._phase)
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
        phase = __import__('inspect').getsource(host.RunnerHost._phase)
        self.assertIn("('wasm_opt_120_round_trip', 'python3 -c ' + shlex.quote(WASM_OPT_PROBE.read_text()))", phase)
        self.assertIn("label + '.' + name, *wasm_opt_check(parsed)", phase)

    def test_parity_checker_remeasures_live_files_and_uses_operator_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp, 'wasm32-unknown-unknown')
            target.mkdir()
            for name in host.PARITY_FILES:
                Path(tmp, name).write_text((host.PARITY_DIR / name).read_text())
            checker = Path(tmp, 'toolchain-parity.py')
            checker.write_text(checker.read_text() + '\ndef check(manifest, role, job, gnu_test_sha256=None, cargo_registry=None):\n'
                               f'    return {{"passed": Path({str(target)!r}).is_dir(), "job": job, "syscalls": len(syscall_surface.expectations(job))}}\n')
            with patch.object(host, 'PARITY_DIR', Path(tmp)):
                source = host.parity_source(job=True)
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
            self.assertTrue(json.loads(output.getvalue())['passed'])
            self.assertEqual(30, json.loads(output.getvalue())['syscalls'])
            target.rmdir()
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
            self.assertFalse(json.loads(output.getvalue())['passed'])
            phase = __import__('inspect').getsource(host.RunnerHost._phase)
            self.assertIn("label + '.toolchain_parity'", phase)
            self.assertIn('("job_toolchain_parity", "cargo --version', phase)

    def test_root_owned_path_check_runs_the_operator_rule_on_boat_roots(self):
        with tempfile.TemporaryDirectory() as tmp:
            checker = Path(tmp, 'root_paths.py')
            checker.write_text('BOAT_ROOTS = ("/fixture",)\n'
                               'BOAT_PROVIDER_RESTORED = {"/fixture/tool": 1000}\n'
                               'def check(roots, provider_files=None, limit=20):\n'
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
        phase = __import__('inspect').getsource(host.RunnerHost._phase)
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
        with tempfile.TemporaryDirectory() as tmp, patch.object(host, 'ROOT', Path(tmp)), \
             patch.object(host, 'job_repository'):
            Path(tmp, '.cortexkit').mkdir()
            with self.assertRaisesRegex(ValueError, 'default sizing'):
                runner.verify_prepared('prepared', Path('.'), 'public', Path('client'))
        self.assertEqual({name: record['status'] for name, record in runner.data['verify_phases'].items()},
                         {'core': 'failed', 'disk-quota': 'failed', 'idle': 'failed'})

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
        jobs = data.pop('list_jobs')
        self.assertEqual(jobs['argv'], ['/usr/bin/systemctl', 'list-jobs', '--no-pager'])
        self.assertEqual(jobs['exit_code'], 0)
        self.assertEqual(set(data), {'motor-boat-restored.service','motor-boat-storage.service','motor-boat-storage-watch.service',r'var-lib-motor\x2dboat.mount','Users.mount','motor-boat.service','motor-boat-host.service','motor-boat-identity.service','motor-boat-bridge.service'})
        for unit, record in data.items():
            self.assertEqual(record['status']['exit_code'], 3)
            self.assertEqual(record['status']['argv'], ['/usr/bin/systemctl','status','--no-pager','-l',unit])
            self.assertEqual(record['journal']['argv'], ['/usr/bin/journalctl','-u',unit,'-b','--no-pager','-n','200'])

    def test_phase_failed_check_captures_units_before_the_next_guest_action(self):
        probe = Mock(data={})
        diagnostics = {'motor-boat-host.service': {'status': 'inactive'}, 'list_jobs': {'stdout': 'no jobs'}}
        probe.execute.return_value = {'exitCode': 0, 'stdout': json.dumps(diagnostics)}
        lane = host.RunnerHost(probe)
        def phase(*args):
            lane.check('initial.armed.endpoint_ready_within_bound', False, {'ready': False})
            self.assertEqual(lane.data['phases'][0]['unit_diagnostics'], diagnostics)
            lane.check('initial.armed.ready_marker_is_exact_id', False, {})
            raise ValueError('endpoint never started')
        with patch.object(lane, '_phase', side_effect=phase), self.assertRaisesRegex(ValueError, 'endpoint never started'):
            lane.phase('sandbox', 'initial', None, None, None, None)
        probe.execute.assert_called_once_with('sandbox', 'sudo -n /usr/bin/python3 -c ' + shlex.quote(host.DIAGNOSTICS), poll_seconds=220)
        self.assertEqual(lane.data['phases'][0]['error'], 'endpoint never started')
        self.assertEqual([row['status'] for row in lane.data['checks']], ['FAIL', 'FAIL'])
        self.assertIsNone(lane._phase_context)

    def test_phase_exception_keeps_original_error_when_diagnostic_collection_fails(self):
        for response in ('unreachable', 'truncated', 'malformed'):
            with self.subTest(response=response):
                probe = Mock(data={})
                if response == 'unreachable':
                    probe.execute.side_effect = RuntimeError('sandbox unreachable')
                elif response == 'truncated':
                    probe.execute.return_value = {'exitCode': 0, 'stdout': '{', 'stdoutTruncated': True}
                else:
                    probe.execute.return_value = {'exitCode': 0, 'stdout': '{'}
                lane = host.RunnerHost(probe)
                with patch.object(lane, '_phase', side_effect=ValueError('original phase error')), self.assertRaisesRegex(ValueError, 'original phase error'):
                    lane.phase('sandbox', 'initial', None, None, None, None)
                record = lane.data['phases'][0]
                self.assertEqual(record['error'], 'original phase error')
                self.assertNotIn('unit_diagnostics', record)
                self.assertTrue('diagnostic_command' in record or 'diagnostics_error' in record)
                probe.execute.assert_called_once()

    def test_successful_phase_does_not_collect_failure_diagnostics(self):
        probe = Mock(data={})
        lane = host.RunnerHost(probe)
        with patch.object(lane, '_phase', return_value={'name': 'initial'}):
            self.assertEqual(lane.phase('sandbox', 'initial', None, None, None, None), {'name': 'initial'})
        probe.execute.assert_not_called()

    def test_inactive_service_reports_state_without_reading_root_cgroup_or_proc_zero(self):
        scope = {}
        exec(host.SERVICE_STATE, scope)
        with patch.object(host.subprocess, 'check_output', return_value='ActiveState=inactive\nSubState=dead\nMainPID=0\nControlGroup=\n'), \
             patch.object(Path, 'read_text', side_effect=AssertionError('inactive unit has no memory cgroup')), \
             patch.object(Path, 'read_bytes', side_effect=AssertionError('PID zero has no process files')):
            state = scope['service_state']('motor-boat-host')
        self.assertEqual(state, {'active_state': 'inactive', 'sub_state': 'dead', 'main_pid': 0, 'cgroup': '',
                                 'memory.current': None, 'memory_error': 'unit has no cgroup: inactive/dead',
                                 'provider_environment_names': [], 'argv': []})

    def test_service_state_samples_running_unit_and_reports_disappearing_processes(self):
        scope = {}
        exec(host.SERVICE_STATE, scope)
        with patch.object(host.subprocess, 'check_output', return_value='ActiveState=active\nSubState=running\nMainPID=123\nControlGroup=/system.slice/motor-boat-host.service\n'), \
             patch.object(Path, 'read_text', return_value='12345\n') as read, \
             patch.object(Path, 'read_bytes', side_effect=[b'HOME=/root\0BOAT_ID=secret\0', b'motor-host\0--sandbox\0/etc/motor/sandbox.json\0']):
            state = scope['service_state']('motor-boat-host')
        self.assertEqual(read.call_count, 1)
        self.assertEqual(state['memory.current'], '12345')
        self.assertEqual(state['main_pid'], 123)
        self.assertEqual(state['provider_environment_names'], ['BOAT_ID'])
        self.assertEqual(state['argv'], ['motor-host', '--sandbox', '/etc/motor/sandbox.json'])
        with patch.object(host.subprocess, 'check_output', return_value='ActiveState=inactive\nSubState=dead\nMainPID=123\nControlGroup=/system.slice/motor-boat-host.service\n'), \
             patch.object(Path, 'read_text', side_effect=FileNotFoundError('cgroup removed')), \
             patch.object(Path, 'read_bytes', side_effect=FileNotFoundError('process exited')):
            state = scope['service_state']('motor-boat-host')
        self.assertIsNone(state['memory.current'])
        self.assertEqual(state['memory_error'], 'cgroup removed')
        self.assertEqual(state['environment_error'], 'process exited')
        self.assertEqual(state['cmdline_error'], 'process exited')

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
        phase = __import__('inspect').getsource(host.RunnerHost._phase)
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

class VerifyPhaseTests(unittest.TestCase):
    """--verify-prepared-image runs each phase in its own sandbox restored from the same image."""

    def runner(self, fail=None):
        probe = Mock()
        probe.data = {}
        probe.size = 'default'
        # These tests exercise phase sequencing only. The live remote compile
        # cache section is a separate operator check with its own tests, so no
        # input directory is supplied here.
        probe.cache_inputs = None
        probe.request.return_value = {'snapshot':{'name':'prepared','status':'ready','snapshotId':'id','sizeBytes':1}}
        sandboxes = iter(['bx_core', 'bx_quota', 'bx_idle'])
        order = []
        def create(name):
            sid = next(sandboxes)
            order.append(('create', name, sid))
            return {'id': sid, 'vcpu': 4, 'memoryGB': 8}, {}
        probe.create.side_effect = create
        probe.wait_hydrated.side_effect = lambda sid: order.append(('hydrated', sid))
        runner = host.RunnerHost(probe)
        def step(name, sid):
            order.append((name, sid))
            if name == fail:
                raise ValueError(name + ' broke')
            runner.check(name + '.ran', True, {})
        def phase(sid, label, *args, **kwargs):
            step(label, sid)
            order.append(('idle_wait_in_phase', kwargs.get('idle', True)))
            return {'node_modules_first': {'result': {'code': 0, 'stdout': 'boat-node-modules',
                                                      'terminal': {'outcome': {'type': 'exit', 'code': 0}}}}}
        runner.phase = Mock(side_effect=phase)
        runner.verify_core_resume = Mock(side_effect=lambda sid, *args: step('core_stop_resume', sid))
        # The live remote compile cache has its own tests; here it records that
        # it ran inside the core phase's sandbox and supplies no extra probes.
        def cache_live(sid, *args):
            step('cache_live', sid)
            runner.data.setdefault('cache_live', {})
        runner.verify_cache_live = Mock(side_effect=cache_live)
        runner.clone_ready = Mock(side_effect=lambda sid, label, *args: step(label, sid))
        def disk_quota(sid, *args):
            step('disk_quota', sid)
            runner.data['disk_quota'] = {'stopped_with_retained_target': True}
        runner.disk_quota = Mock(side_effect=disk_quota)
        runner.release_retained_job = Mock(side_effect=lambda sid: step('release_retained_job', sid))
        runner.replay_cleanup = Mock(side_effect=lambda sid: step('cleanup_replay', sid))
        runner.verify_idle = Mock(side_effect=lambda sid, *args: step('idle', sid))
        runner.release_sandbox = Mock(side_effect=lambda phase, record: order.append(('released', record['sandbox'])))
        runner.execute = Mock(side_effect=AssertionError('must not reconfigure guest'))
        return runner, probe, order

    def verify(self, runner, phases=None):
        with tempfile.TemporaryDirectory() as tmp, patch.object(host, 'ROOT', Path(tmp)), \
             patch.object(host, 'job_repository'), patch.dict(sys.modules, {'probe': Mock(READY={'running'})}):
            Path(tmp, '.cortexkit').mkdir()
            if phases is None:
                runner.verify_prepared('prepared', Path('.'), 'public', Path('client'))
            else:
                runner.verify_prepared('prepared', Path('.'), 'public', Path('client'), phases)

    def statuses(self, runner):
        return {name: record['status'] for name, record in runner.data['verify_phases'].items()}

    def test_no_phase_runs_every_phase_in_its_own_sandbox_and_releases_each(self):
        runner, probe, order = self.runner()
        self.verify(runner)
        self.assertEqual(order, [
            ('create', 'prepared', 'bx_core'), ('hydrated', 'bx_core'), ('prepared_clone', 'bx_core'),
            ('idle_wait_in_phase', False), ('cache_live', 'bx_core'), ('core_stop_resume', 'bx_core'),
            ('released', 'bx_core'),
            ('create', 'prepared', 'bx_quota'), ('hydrated', 'bx_quota'), ('disk_quota_clone', 'bx_quota'),
            ('disk_quota', 'bx_quota'), ('hydrated', 'bx_quota'), ('release_retained_job', 'bx_quota'),
            ('resumed', 'bx_quota'), ('idle_wait_in_phase', True),
            ('cleanup_replay', 'bx_quota'), ('released', 'bx_quota'),
            ('create', 'prepared', 'bx_idle'), ('hydrated', 'bx_idle'), ('idle', 'bx_idle'), ('released', 'bx_idle')])
        self.assertEqual(self.statuses(runner), {'core': 'passed', 'disk-quota': 'passed', 'idle': 'passed'})
        self.assertEqual(runner.data['verify_phases']['core']['check_names'],
                         ['prepared_clone.ran', 'cache_live.ran', 'core_stop_resume.ran'])
        self.assertIn('disk_quota.normal_job_after_stop_resume', runner.data['verify_phases']['disk-quota']['check_names'])
        runner.execute.assert_not_called()
        # The control method is mocked here; separate tests use mocked provider/client calls to exercise its sequence.
        posts = [call.args[1] for call in probe.request.call_args_list if call.args[0] == 'POST']
        self.assertEqual(posts, ['/sandboxes/bx_quota/resume'])
        timed = [row['name'] for row in runner.data['section_timings']]
        for name in ('core', 'core/create', 'core/hydrate', 'disk-quota/disk_quota', 'disk-quota/resume',
                     'disk-quota/cleanup_replay', 'idle'):
            self.assertIn(name, timed)
        self.assertTrue(all(row['outcome'] == 'completed' and row['seconds'] is not None
                            for row in runner.data['section_timings']))

    def test_one_phase_runs_alone_and_the_others_are_not_run_never_passed(self):
        for phase in host.VERIFY_PHASES:
            with self.subTest(phase=phase):
                runner, probe, order = self.runner()
                self.verify(runner, (phase,))
                self.assertEqual(probe.create.call_count, 1)
                self.assertEqual(runner.verify_core_resume.call_count, int(phase == 'core'))
                # The live remote compile-cache section belongs to the core
                # phase's sandbox and is a no-op without --cache-inputs.
                self.assertEqual(runner.verify_cache_live.call_count, int(phase == 'core'))
                if phase == 'core':
                    runner.disk_quota.assert_not_called()
                statuses = self.statuses(runner)
                self.assertEqual(statuses.pop(phase), 'passed')
                self.assertEqual(set(statuses.values()), {'not_run'})
                for name in statuses:
                    record = runner.data['verify_phases'][name]
                    self.assertEqual((record['requested'], record['ran']), (False, False))
                    self.assertNotIn('check_names', record)

    def test_a_failed_phase_is_recorded_the_next_still_runs_and_the_run_fails(self):
        runner, probe, order = self.runner(fail='prepared_clone')
        with self.assertRaisesRegex(ValueError, 'verify phase failed: core: prepared_clone broke'):
            self.verify(runner)
        self.assertEqual(self.statuses(runner), {'core': 'failed', 'disk-quota': 'passed', 'idle': 'passed'})
        self.assertEqual(runner.data['verify_phases']['core']['error'], 'prepared_clone broke')
        self.assertEqual(probe.create.call_count, 3)
        self.assertIn(('released', 'bx_core'), order)
        core = next(row for row in runner.data['section_timings'] if row['name'] == 'core')
        self.assertEqual((core['outcome'], core['error']), ('error', 'prepared_clone broke'))

    def test_failed_plain_control_blocks_the_core_phase(self):
        runner, probe, order = self.runner(fail='core_stop_resume')
        with self.assertRaisesRegex(ValueError, 'verify phase failed: core: core_stop_resume broke'):
            self.verify(runner, ('core',))
        self.assertEqual(self.statuses(runner), {'core': 'failed', 'disk-quota': 'not_run', 'idle': 'not_run'})
        self.assertIn(('core_stop_resume', 'bx_core'), order)
        self.assertIn(('released', 'bx_core'), order)

    def test_a_size_refusal_stops_every_later_phase(self):
        runner, probe, order = self.runner()
        def refused(name):
            probe.data['snapshot_size_bound'] = {'requested_size': 'default'}
            raise RuntimeError('Boat refused the size')
        probe.create.side_effect = refused
        with self.assertRaisesRegex(RuntimeError, 'refused'):
            self.verify(runner)
        self.assertEqual(self.statuses(runner), {'core': 'snapshot_size_bound', 'disk-quota': 'not_run', 'idle': 'not_run'})
        runner.release_sandbox.assert_not_called()

    def test_each_phase_restarts_the_wall_clock_budget(self):
        runner, probe, order = self.runner()
        runner.budget_seconds = 1800
        deadlines = []
        runner.release_sandbox = Mock(side_effect=lambda phase, record: deadlines.append(probe.deadline))
        clock = __import__('itertools').count()
        with patch.object(host.time, 'monotonic', side_effect=lambda: float(next(clock))):
            self.verify(runner)
        self.assertEqual(len(deadlines), 3)
        self.assertTrue(deadlines[0] < deadlines[1] < deadlines[2])
        self.assertTrue(all(deadline >= 1800 for deadline in deadlines))

    def test_the_core_phase_leaves_the_idle_wait_to_the_idle_phase(self):
        source = __import__('inspect').getsource(host.RunnerHost._phase)
        self.assertIn("if label in ('prepared_clone', 'snapshot_clone') and idle:", source)
        idle = __import__('inspect').getsource(host.RunnerHost.verify_idle)
        self.assertIn('self.idle_reclaim_wait(sid, label, record)', idle)
        self.assertIn("self.check_job(label + '.after_idle', parsed, 0, marker='boat-node-modules')", idle)


class CoreStopResumeTests(unittest.TestCase):
    def runner(self, fault=None):
        probe = Mock()
        probe.data = {}
        probe.deadline = 1800
        probe.quota_stop_wait_s = 1500
        order, clock = [], [0.0]
        runner = host.RunnerHost(probe)
        runner.unit_diagnostics = Mock()
        runner.disk_quota = Mock()
        idle = {'ready': True, 'leases': ['active'] if fault == 'busy' else [],
                'root_free_bytes': 50 * 1024**3, 'xfs_free_bytes': 60 * 1024**3}
        observations = iter([idle, {'backing_allocated_blocks': 1024}, {'restored_storage': True}])
        def guest_json(_sid, _command, label):
            order.append(label)
            return next(observations)
        runner.guest_json = Mock(side_effect=guest_json)
        def request(method, path, body):
            self.assertEqual((method, body), ('POST', {}))
            order.append(path.rsplit('/', 1)[-1])
            if path.endswith('/stop'):
                clock[0] += 5
                return {'ok': True, 'type': 'sandbox.stopping', 'status': 'archiving'}
            return {'ok': True}
        probe.request.side_effect = request
        def wait(_sid, states, **kwargs):
            if 'archived' in states:
                order.append('archived')
                clock[0] += 7.5
                return {'state': 'archived', 'stop': {'status': 'pending' if fault == 'pending' else 'completed'}}
            order.append('ready')
            return {'state': 'ready'}
        probe.wait_sandbox.side_effect = wait
        probe.wait_hydrated.side_effect = lambda sid: order.append('hydrated')
        runner.route = Mock(return_value=['ssh'])
        def readiness(sid, label, record, hydrated_at):
            order.append('endpoint_ready')
            runner.check(label + '.endpoint_ready_within_bound', fault != 'not-ready', {})
        runner.existing_then_reinstall = Mock(side_effect=readiness)
        def execute(sid, command):
            self.assertEqual(command, 'sudo -n systemctl start motor-boat-bridge')
            order.append('bridge')
            return {'stdout': '', 'exitCode': 0}
        runner.execute = Mock(side_effect=execute)
        return runner, probe, order, clock

    @contextlib.contextmanager
    def fakes(self, order, clock, code=0):
        def forward(argv, directory, deadline, record):
            record['reaped'] = True
            return contextlib.nullcontext('address')
        def job(argv, **kwargs):
            order.append('job')
            self.assertEqual(argv[4], host.NODE_CHECK)
            result = {'code': code, 'stdout': 'boat-node-modules',
                      'terminal': {'outcome': {'type': 'exit', 'code': code}}}
            return {'exit_code': 0, 'stdout': json.dumps(result)}
        with patch.object(host.time, 'monotonic', side_effect=lambda: clock[0]), \
             patch.object(host, 'tunnel', side_effect=forward), patch.object(host, 'process', side_effect=job) as client, \
             patch.object(host, 'storage_checks', return_value={'storage_ok': True}):
            yield client

    def test_plain_control_archives_idle_clone_then_resumes_endpoint_and_real_job(self):
        runner, probe, order, clock = self.runner()
        with self.fakes(order, clock) as client:
            runner.verify_core_resume('bx_core', Path('.'), 'public', Path('.'), Path('client'))
        self.assertEqual(order, ['core idle endpoint before stop', 'core pre-stop storage', 'stop', 'archived',
                                 'resume', 'ready', 'hydrated', 'endpoint_ready', 'bridge', 'job', 'core resumed job storage'])
        runner.disk_quota.assert_not_called()
        self.assertEqual(runner.data['core_stop_resume']['accepted_to_completed_seconds'], 7.5)
        self.assertEqual(len(probe.wait_sandbox.call_args_list[0].args), 2)
        self.assertEqual(probe.wait_sandbox.call_args_list[0].args, ('bx_core', {'archived'}))
        runner.existing_then_reinstall.assert_called_once()
        client.assert_called_once()
        checks = {row['name']: row['status'] for row in runner.data['checks']}
        self.assertEqual(checks['core_stop_resume.idle_before_stop'], 'PASS')
        self.assertEqual(checks['core_stop_resume.stop_completed'], 'PASS')
        self.assertEqual(checks['core_stop_resume.normal_job_after_resume'], 'PASS')
        self.assertTrue(all(status == 'PASS' for status in checks.values()))

    def test_busy_endpoint_never_sends_the_control_stop(self):
        runner, probe, order, clock = self.runner('busy')
        with self.fakes(order, clock) as client, self.assertRaisesRegex(ValueError, 'no job quota leases'):
            runner.verify_core_resume('bx_core', Path('.'), 'public', Path('.'), Path('client'))
        probe.request.assert_not_called()
        client.assert_not_called()

    def test_incomplete_archival_or_unready_endpoint_prevents_resumed_job(self):
        for fault in ('pending', 'not-ready'):
            with self.subTest(fault=fault):
                runner, probe, order, clock = self.runner(fault)
                with self.fakes(order, clock) as client, self.assertRaises(ValueError):
                    runner.verify_core_resume('bx_core', Path('.'), 'public', Path('.'), Path('client'))
                client.assert_not_called()
                self.assertTrue(any(row['status'] == 'FAIL' for row in runner.data['checks']))
                if fault == 'pending':
                    self.assertNotIn('resume', order)

    def test_failed_resumed_job_is_a_required_core_failure(self):
        runner, probe, order, clock = self.runner()
        with self.fakes(order, clock, code=1):
            runner.verify_core_resume('bx_core', Path('.'), 'public', Path('.'), Path('client'))
        check = next(row for row in runner.data['checks'] if row['name'] == 'core_stop_resume.normal_job_after_resume')
        self.assertEqual(check['status'], 'FAIL')
        self.assertEqual(host.verify_phase_status({'ran': True, 'completed': True}, runner.data['checks']), 'failed')


class SectionTimingTests(unittest.TestCase):
    def test_sections_nest_record_seconds_and_stay_visible_while_running(self):
        probe = Mock()
        probe.data = {}
        runner = host.RunnerHost(probe)
        seen = []
        probe.checkpoint.side_effect = lambda: seen.append([dict(row) for row in runner.data.get('section_timings', [])])
        with runner.section('core'):
            with runner.section('create'):
                pass
            with self.assertRaises(ValueError):
                with runner.section('hydrate'):
                    raise ValueError('timed out waiting for sandbox')
        rows = {row['name']: row for row in runner.data['section_timings']}
        self.assertEqual(list(rows), ['core', 'core/create', 'core/hydrate'])
        self.assertEqual(rows['core/hydrate']['outcome'], 'error')
        self.assertIn('timed out', rows['core/hydrate']['error'])
        self.assertEqual((rows['core']['outcome'], rows['core/create']['outcome']), ('completed', 'completed'))
        self.assertTrue(all(isinstance(row['seconds'], float) for row in rows.values()))
        # The first checkpoint already shows the step as running, so a killed run names it.
        self.assertEqual(seen[0], [dict(name='core', started_at=seen[0][0]['started_at'], seconds=None, outcome='running')])
        self.assertEqual(runner._sections, [])


class RetainedQuotaSnapshotTests(unittest.TestCase):
    def test_pending_stop_samples_use_pinned_ssh_and_preserve_unknown_observations(self):
        probe = Mock()
        probe.data = {}
        probe.deadline = host.time.monotonic() + 1800
        runner = host.RunnerHost(probe)
        child = Mock()
        child.poll.return_value = None
        guest = {'job_running': True, 'allocated_bytes': 2**30, 'retained_target_at_limit': True,
                 'root_free_bytes': 50 * 2**30, 'xfs_free_bytes': 40 * 2**30}
        with patch.object(host, 'process', return_value={'exit_code': 0, 'stdout': json.dumps(guest)}) as ssh:
            observed = runner.retained_stop_sample({'state': 'archiving'}, {}, ['ssh', 'pinned-host'], child)
            self.assertEqual(observed['status'], 'observed')
            self.assertTrue(observed['job_running'])
            self.assertTrue(observed['client_running'])
            self.assertEqual(observed['allocated_bytes'], 2**30)
            self.assertEqual(ssh.call_args.args[0][:2], ['ssh', 'pinned-host'])
            self.assertIn('sudo -n /usr/bin/python3 -c', ssh.call_args.args[0][-1])
            self.assertLessEqual(ssh.call_args.kwargs['timeout'], 10)
        for response in ({'exit_code': 255, 'stderr': 'SSH unavailable'}, {'exit_code': 0, 'stdout': '{}'}):
            with self.subTest(response=response), patch.object(host, 'process', return_value=response):
                observed = runner.retained_stop_sample({'state': 'archiving'}, {}, ['ssh', 'pinned-host'], child)
                self.assertEqual(observed['status'], 'unavailable')
                self.assertNotIn('job_running', observed)
        with patch.object(host, 'process') as ssh:
            observed = runner.retained_stop_sample({'state': 'archived', 'stop': {'status': 'completed'}}, {}, ['ssh'], child)
            self.assertEqual(observed['status'], 'archived')
            ssh.assert_not_called()
        probe.execute.assert_not_called()
        probe.request.assert_not_called()

    def test_completed_stop_cannot_qualify_a_dead_or_unobserved_retained_job(self):
        case = json.loads(Path(__file__).with_name('fixtures').joinpath('full-quota-stops.json').read_text())['default']
        for guest, expected, phase in (({'status': 'observed', 'job_running': False, 'retained_target_at_limit': True}, 'FAIL', 'failed'),
                                       ({'status': 'unavailable', 'error': 'SSH unavailable'}, 'INCONCLUSIVE', 'inconclusive'),
                                       ({'status': 'observed', 'job_running': True, 'retained_target_at_limit': True}, 'PASS', 'passed')):
            with self.subTest(expected=expected):
                probe = Mock()
                probe.data = {}
                probe.quota_stop_wait_s = 1500
                probe.request.return_value = case['stop_response']
                def wait(sid, states, seconds, *, observation, on_sample):
                    observation['samples'] = [{'sandbox': {'state': 'archiving'}, 'retained_job': guest},
                                               {'sandbox': {'state': 'archived', 'stop': {'status': 'completed'}},
                                                'retained_job': {'status': 'archived'}}]
                    return {'state': 'archived', 'stop': {'status': 'completed'}}
                probe.wait_sandbox.side_effect = wait
                runner = host.RunnerHost(probe)
                runner.stop_at_quota_limit(case['sandbox_id'], {}, case)
                check = next(row for row in runner.data['checks'] if row['name'] == 'disk_quota.retained_target_held_during_stop')
                self.assertEqual(check['status'], expected)
                self.assertEqual(host.verify_phase_status({'ran': True, 'completed': True}, runner.data['checks']), phase)

    def test_quota_stop_forwards_its_configured_wait_cap(self):
        case = json.loads(Path(__file__).with_name('fixtures').joinpath('full-quota-stops.json').read_text())['default']
        probe = Mock()
        probe.data = {}
        probe.quota_stop_wait_s = 1500
        probe.request.return_value = case['stop_response']
        probe.wait_sandbox.return_value = {'state': 'archived', 'stop': {'status': 'completed'}}
        runner = host.RunnerHost(probe)
        record = {}
        runner.stop_at_quota_limit(case['sandbox_id'], record, case)
        probe.wait_sandbox.assert_called_once_with(case['sandbox_id'], {'archived'}, 1500,
                                                   observation=record['stop_at_limit']['wait'], on_sample=None)
        self.assertEqual(runner.data['checks'][-1]['status'], 'PASS')

    def test_recorded_full_quota_stop_timeout_names_state_operation_and_disks(self):
        from probe import Probe
        recorded = json.loads(Path(__file__).with_name('fixtures').joinpath('full-quota-stops.json').read_text())
        for size, count, backing_bytes in (('default', 390, 13225811968), ('large', 388, 51881713664)):
            with self.subTest(size=size):
                case = recorded[size]
                sid = case['sandbox_id']
                polls = []
                for group in case['poll_groups']:
                    state = {'id': sid, 'state': 'archiving', 'stop': case['stop_response']['operation'],
                             **{key: value for key, value in group.items() if key != 'count'}}
                    polls.extend([{'ok': True, 'type': 'sandbox.info', 'sandbox': state}] * group['count'])
                self.assertEqual(len(polls), count)
                client = Mock()
                response = dict(case['stop_response'], sandbox=polls[0]['sandbox'])
                client.request.side_effect = [response] + polls
                probe = Probe(client, {'run_id': 'recorded-stop'}, Mock())
                probe.deadline = 1800
                runner = host.RunnerHost(probe)
                record = {}
                clock = [0.0]
                def advance(_seconds):
                    clock[0] += 901 / count
                with patch('probe.time.monotonic', side_effect=lambda: clock[0]), patch('probe.time.sleep', side_effect=advance):
                    with self.assertRaisesRegex(ValueError, '^disk_quota.stop_with_quota_sized_target:'):
                        runner.stop_at_quota_limit(sid, record, case)
                failure = next(row for row in runner.data['checks'] if row['name'] == 'disk_quota.stop_with_quota_sized_target')
                self.assertEqual((failure['name'], failure['status']), ('disk_quota.stop_with_quota_sized_target', 'FAIL'))
                wait = failure['evidence']['wait']
                self.assertEqual(wait['polls'], count)
                self.assertGreaterEqual(wait['seconds'], 900)
                self.assertEqual(wait['last_sandbox']['state'], 'archiving')
                self.assertEqual(wait['last_sandbox']['stop']['status'], 'pending')
                self.assertIsNone(wait['last_sandbox']['stop']['lastAttemptAt'])
                self.assertEqual(wait['last_sandbox']['lastSnapshotStatus'], case['poll_groups'][-1]['lastSnapshotStatus'])
                self.assertEqual(failure['evidence']['response'], response)
                disks = failure['evidence']['pre_stop_disk']
                self.assertEqual(disks['root_free_bytes'], case['pre_stop']['root_free_bytes'])
                self.assertEqual(disks['xfs_free_bytes'], case['pre_stop']['xfs_free_bytes'])
                self.assertEqual(disks['backing_allocated_bytes'], backing_bytes)
                for value in ('archiving', 'pending', case['stop_response']['operation']['id'],
                              str(disks['root_free_bytes']), str(disks['xfs_free_bytes'])):
                    self.assertIn(value, failure['reason'])
                client.request.assert_any_call('POST', f'/sandboxes/{sid}/stop', {})
                self.assertEqual(client.request.call_count, count + 1)
                self.assertNotIn('stopped_with_retained_target', record)
                self.assertEqual(runner.data['section_timings'][-1]['outcome'], 'error')

    def test_snapshot_stops_with_data_held_and_never_on_missing_charge(self):
        import disk_quota
        for valid in (True, False):
            with self.subTest(valid=valid):
                probe = Mock()
                probe.data = {}
                probe.quota_stop_wait_s = 900
                probe.deadline = 10**12
                runner = host.RunnerHost(probe)
                runner.route = Mock(return_value=['ssh'])
                runner.check_job = Mock()
                control = dict(request=0x401c5820, clear_succeeded=True, before_flags=0x200, after_flags=0,
                               before_project=0, after_project=0, file_setattr_supported=True, file_setattr_errno=0)
                observation = dict(errno=errno.EDQUOT, lease=dict(blocks=2**21, growth_bytes=2**30),
                                   kernel_usage=dict(hard_bytes=2**30, used_bytes=2**30),
                                   before=dict(physical_allowance_bytes=2**30), retained_allocated_bytes=2**30 if valid else 0,
                                   root_free_bytes=disk_quota.RESERVE_BYTES, xfs_free_bytes=disk_quota.RESERVE_BYTES)
                order = []
                responses = iter([control, dict(ready=True, leases=[]), observation,
                                  dict(job_running=True, allocated_bytes=2**30),
                                  dict(expected_backing=True, backing_file='/var/lib/motor-boat.xfs', backing_allocated_blocks=1000)])
                def execute(sid, command, **kwargs):
                    if '--show-cursor' in command:
                        order.append('journal_cursor')
                        return {'stdout': '-- cursor: saved-before-stop\n'}
                    if 'backing_allocated_blocks' in command:
                        order.append('backing_allocation')
                    return {'stdout': json.dumps(next(responses))}
                runner.execute = Mock(side_effect=execute)
                probe.request.side_effect = lambda *args: order.append('stop') or {}
                probe.wait_sandbox.side_effect = lambda *args, **kwargs: order.append('archived')
                child = Mock(returncode=-15)
                child.poll.return_value = None
                child.communicate.return_value = ('', 'client interrupted')
                child.terminate.side_effect = lambda: order.append('terminate')
                with patch.object(disk_quota, 'LANES', ()), patch.object(host, 'tunnel', return_value=contextlib.nullcontext('address')), \
                     patch.object(host, 'process', return_value={'exit_code': 0, 'stdout': json.dumps(dict(code=0, terminal={}, stdout='deep'))}), \
                     patch.object(host, '_quota_popen', return_value=child):
                    if valid:
                        runner.disk_quota('bx_test', Path('.'), 'public', Path('.'), Path('client'))
                        self.assertEqual(order, ['backing_allocation', 'journal_cursor', 'stop', 'archived', 'terminate'])
                        self.assertTrue(runner.data['disk_quota']['stopped_with_retained_target'])
                        self.assertEqual(runner.data['disk_quota']['lanes']['retained_target']['eviction_journal_cursor'], 'saved-before-stop')
                        self.assertFalse(runner.data['disk_quota']['reserve_assurance']['sufficiency_proven'])
                    else:
                        with self.assertRaisesRegex(ValueError, 'retained target'):
                            runner.disk_quota('bx_test', Path('.'), 'public', Path('.'), Path('client'))
                        probe.request.assert_not_called()
                    self.assertEqual(host._quota_popen.call_args.args[0][-1], '1800')
                    row = runner.data['disk_quota']['lanes']['retained_target']
                    self.assertEqual((row['hold_seconds'], row['job_timeout_seconds']), (1800, 1800))
                self.assertTrue(any('HOLD_AT_LIMIT=True' in call.args[1] for call in runner.execute.call_args_list))

    def test_resume_records_and_checks_backing_allocation_drop(self):
        # The drop must cover the evicted target's allocation, less the slack;
        # any smaller allocation used to pass (see
        # RecordedC7c6127fTests.test_recorded_two_mib_backing_shrink_is_not_a_reclaimed_target).
        import disk_quota
        retained = 4 * 1024**3
        for blocks, passed in ((1000 + 1024**3 // 512, True), (1000 + 1024**3 // 512 + 1, False),
                               (100 + 4 * 1024**3 // 512, False), (None, False)):
            with self.subTest(blocks=blocks):
                probe = Mock()
                before = dict(expected_backing=True, backing_file='/var/lib/motor-boat.xfs',
                              backing_allocated_blocks=1000 + retained // 512)
                probe.data = {}
                runner = host.RunnerHost(probe)
                runner.data['disk_quota'] = {'lanes': {disk_quota.RETAINED_LANE: {
                    'pre_stop_storage': before, 'at_limit': {'retained_allocated_bytes': retained}}}}
                runner.check = Mock()
                after = dict(before, backing_allocated_blocks=blocks)
                runner.quota_reclaimed(after)
                name, observed, evidence = runner.check.call_args.args
                self.assertEqual((name, observed), ('disk_quota.backing_allocation_dropped_after_resume', passed))
                self.assertEqual(evidence, {'before': before, 'after': after, 'retained_allocated_bytes': retained})
                self.assertEqual(runner.check.call_args.kwargs['reason'] is None, passed)


class RecordedDiskQuotaPollTests(unittest.TestCase):
    """Replays the Boat command results that stopped the disk-quota phase of the
    prepared-image verify at revision 458c6333 (fixtures/disk-quota-boat-polls.json)."""

    @staticmethod
    def recorded():
        return json.loads(Path(__file__).with_name('fixtures').joinpath('disk-quota-boat-polls.json').read_text())

    def replay(self, size, lanes, responses):
        """Run the disk-quota phase with the recorded guest results; `responses`
        maps a predicate on the guest command to its recorded Boat status."""
        import disk_quota
        recorded = self.recorded()[size]
        probe = Mock()
        probe.data = {}
        probe.quota_stop_wait_s = 900
        probe.deadline = 10**12
        runner = host.RunnerHost(probe)
        runner.route = Mock(return_value=['ssh'])
        runner.check_job = Mock()
        cleanups = iter(responses.pop('cleanup'))
        def ok(value):
            return {'exitCode': 0, 'stdout': json.dumps(value) + '\n', 'stderr': '', 'status': 'exited'}
        def execute(sid, command, **kwargs):
            if shlex.quote(disk_quota.POSITIVE_CONTROL) in command:
                return ok(recorded['setter_positive_control'])
            if shlex.quote(disk_quota.CLEANUP) in command:
                return next(cleanups)
            for matches, response in responses.items():
                if matches in command:
                    return response
            raise AssertionError('unexpected guest command: ' + command[:200])
        runner.execute = Mock(side_effect=execute)
        child = Mock(returncode=0)
        child.poll.return_value = 0
        return recorded, runner, probe, child, patch.object(disk_quota, 'LANES', lanes), \
            patch.object(host, 'tunnel', return_value=contextlib.nullcontext('address')), \
            patch.object(host, '_quota_popen', return_value=child), patch.object(host, 'EMPTY_OUTPUT_REPOLL_SECONDS', 0, create=True)

    def test_recorded_large_empty_cleanup_poll_is_a_named_failure(self):
        # Large run: the reused_target job finished and was recorded, then Boat
        # reported the cleanup wait (process 39139) exited 0 with empty stdout.
        recorded = self.recorded()['large']
        poll = recorded['reused_target_cleanup_poll']
        self.assertEqual((poll['status'], poll['exitCode'], poll['stdout'], poll['stderr']), ('exited', 0, '', ''))
        deep = {'exitCode': 0, 'stdout': json.dumps(recorded['deep_cleanup']) + '\n', 'status': 'exited'}
        observer = {'exitCode': 0, 'stdout': json.dumps(recorded['reused_target_at_limit']) + '\n', 'status': 'exited'}
        _, runner, probe, child, *patches = self.replay('large', ('reused_target',), {
            'cleanup': [deep, poll], 'EXPECTED_TOKEN=': observer})
        client = recorded['reused_target_client']
        child.communicate.return_value = (client['stdout'], client['stderr'])
        probe.command_status.return_value = poll
        with patches[0], patches[1], patches[2], patches[3], \
             patch.object(host, 'process', return_value=recorded['baseline_seed_client']):
            with self.assertRaisesRegex(ValueError, r'^disk_quota cleanup after lane reused_target: guest command exited 0 '
                                                    r'with empty stdout .*5 re-polls of Boat process 39139'):
                runner.disk_quota(recorded['sandbox'], Path('.'), 'public', Path('.'), Path('client'))
        self.assertEqual(probe.command_status.call_count, host.EMPTY_OUTPUT_REPOLLS)
        probe.command_status.assert_called_with(recorded['sandbox'], 39139)
        row = runner.data['disk_quota']['lanes']['reused_target']
        self.assertEqual(row['result']['code'], 0)
        self.assertNotIn('cleanup', row)
        failure = runner.data['guest_failures'][-1]
        self.assertEqual((failure['label'], failure['reason']), ('disk_quota cleanup after lane reused_target', 'empty stdout'))
        self.assertEqual(runner.data['empty_output_repolls'][-1]['output_arrived'], False)

    def test_late_guest_output_after_an_empty_exited_poll_is_used_and_recorded(self):
        poll = self.recorded()['large']['reused_target_cleanup_poll']
        probe = Mock()
        probe.data = {}
        runner = host.RunnerHost(probe)
        runner.execute = Mock(return_value=poll)
        late = dict(poll, stdout='{"ready": true, "leases": []}\n')
        probe.command_status.side_effect = [poll, late]
        with patch.object(host, 'EMPTY_OUTPUT_REPOLL_SECONDS', 0):
            self.assertEqual(runner.guest_json('bx', 'cleanup', 'cleanup step'), {'ready': True, 'leases': []})
        self.assertEqual(probe.command_status.call_count, 2)
        self.assertEqual(runner.data['empty_output_repolls'][0]['output_arrived'], True)
        self.assertNotIn('guest_failures', runner.data)
        # Output that is present but not JSON is named too, with what was printed.
        runner.execute = Mock(return_value=dict(poll, stdout='Traceback\n'))
        with self.assertRaisesRegex(ValueError, r"^cleanup step: guest command stdout is not JSON .*'Traceback"):
            runner.guest_json('bx', 'cleanup', 'cleanup step')

    def test_recorded_default_journal_cursor_after_no_entries_line_is_used(self):
        # Default run: journalctl printed "-- No entries --" (it always does
        # with --lines=0) and then the cursor, on the second line.
        recorded = self.recorded()['default']
        poll = recorded['journal_cursor_poll']
        self.assertTrue(poll['stdout'].startswith('-- No entries --\n-- cursor: '))
        def ok(value):
            return {'exitCode': 0, 'stdout': json.dumps(value) + '\n', 'status': 'exited'}
        _, runner, probe, child, *patches = self.replay('default', (), {
            'cleanup': [ok(recorded['deep_cleanup'])],
            'EXPECTED_TOKEN=': ok(recorded['retained_target_at_limit']),
            'OBSERVATION=': ok(recorded['retained_target_pre_stop']),
            shlex.quote(host.STORAGE_STATE): ok(recorded['retained_target_pre_stop_storage']),
            '--show-cursor': poll})
        child.poll.return_value = None
        child.communicate.return_value = ('', 'client interrupted')
        probe.request.return_value = {}
        with patches[0], patches[1], patches[2], patches[3], \
             patch.object(host, 'process', return_value={'exit_code': 0, 'stdout': json.dumps(dict(code=0, terminal={}, stdout='deep'))}):
            runner.disk_quota(recorded['sandbox'], Path('.'), 'public', Path('.'), Path('client'))
        record = runner.data['disk_quota']
        self.assertTrue(record['stopped_with_retained_target'])
        self.assertEqual(record['lanes']['retained_target']['eviction_journal_cursor'],
                         's=4c4ce718decd416491c455c60388f086;i=2000;b=bc54cabec54f4ac0a1532d161f94134d;'
                         'm=1dce7402;t=65e9e1ed82b65;x=f298a0d395997b89')
        probe.request.assert_called_once_with('POST', f"/sandboxes/{recorded['sandbox']}/stop", {})

    def test_missing_or_ambiguous_journal_cursor_names_what_journalctl_printed(self):
        for stdout in ('-- No entries --\n', '', None, '-- cursor: \n', '-- cursor: a\n-- cursor: b\n'):
            with self.subTest(stdout=stdout):
                with self.assertRaisesRegex(ValueError, r'did not provide exactly one pre-stop eviction cursor line.*journalctl printed '
                                            + re.escape(repr(stdout))):
                    host.journal_cursor(stdout)
        self.assertEqual(host.journal_cursor('-- cursor: s=1;i=2\n'), 's=1;i=2')


class RecordedC7c6127fTests(unittest.TestCase):
    """The phased prepared-image verify at c7c6127f (fixtures/boat-verify-c7c6127f.json)."""

    @staticmethod
    def recorded():
        return json.loads(Path(__file__).with_name('fixtures').joinpath('boat-verify-c7c6127f.json').read_text())

    def resume_runner(self, release):
        """A disk-quota phase that stopped with the recorded retained job, with
        every Boat and guest call recorded in `order`."""
        import disk_quota
        case = self.recorded()['default_resume']
        probe = Mock()
        probe.data = {}
        probe.deadline = host.time.monotonic() + 1800
        order = []
        runner = host.RunnerHost(probe)
        runner.clone_ready = Mock()
        def stopped(*args):
            runner.data['disk_quota'] = {'stopped_with_retained_target': True,
                                         'lanes': {disk_quota.RETAINED_LANE: {'at_limit': case['retained_at_limit']}}}
        runner.disk_quota = Mock(side_effect=stopped)
        probe.request.side_effect = lambda method, path, body: order.append(('request', path)) or {}
        probe.wait_sandbox.side_effect = lambda *args, **kwargs: order.append(('wait_sandbox',))
        probe.wait_hydrated.side_effect = lambda *args: order.append(('hydrated',))
        def guest_json(sid, command, label, timeout=120):
            order.append(('guest', label, host.time.monotonic()))
            return release
        runner.guest_json = Mock(side_effect=guest_json)
        def phase(sid, label, *args, hydrated_at=None, **kwargs):
            order.append(('phase', label, hydrated_at))
            return {'node_modules_first': {'result': {'code': 0}}}
        runner.phase = Mock(side_effect=phase)
        runner.check_job = Mock()
        runner.replay_cleanup = Mock()
        return runner, order, case

    def test_recorded_resumed_retained_job_is_released_before_readiness_is_timed(self):
        # At 04:27:48, after the resume, the retained job's unit was still in
        # the runner's cgroup: Boat had restored memory (same boot id), so the
        # job still held its barrier and the runner kept its workspace.
        case = self.recorded()['default_resume']
        self.assertEqual(*case['boot_id_before_and_after'])
        self.assertFalse(case['workspace_eviction']['workspace_absent'])
        release = {'released': True, 'job_terminal': True, 'unit_active': False, 'terminal_before': False}
        runner, order, _ = self.resume_runner(release)
        runner.verify_disk_quota('bx_a6kvzryf', Path('.'), 'public', Path('.'), Path('client'))
        steps = [step[0] if step[0] != 'guest' else step[1] for step in order]
        self.assertEqual(steps, ['request', 'wait_sandbox', 'hydrated', 'retained job release after resume', 'phase'])
        self.assertEqual(order[0][1], '/sandboxes/bx_a6kvzryf/resume')
        command = runner.guest_json.call_args.args[1]
        self.assertIn(case['retained_at_limit']['probe_token'], command)
        self.assertIn(case['retained_at_limit']['job_id'], command)
        # Readiness is timed from the release, not from hydration.
        self.assertGreaterEqual(order[-1][2], order[-2][2])
        check = next(row for row in runner.data['checks'] if row['name'] == 'disk_quota.retained_job_ended_after_resume')
        self.assertEqual(check['status'], 'PASS')

    def test_retained_job_that_does_not_end_after_release_is_a_named_failure(self):
        for release in ({'released': True, 'job_terminal': False, 'unit_active': True, 'terminal_before': False},
                        {'released': False, 'marker_error': 'absent', 'job_terminal': False, 'unit_active': True}):
            with self.subTest(release=release):
                runner, order, _ = self.resume_runner(release)
                with self.assertRaisesRegex(ValueError, '^disk_quota.retained_job_ended_after_resume: '):
                    runner.verify_disk_quota('bx_a6kvzryf', Path('.'), 'public', Path('.'), Path('client'))
                runner.phase.assert_not_called()
                check = next(row for row in runner.data['checks'] if row['name'] == 'disk_quota.retained_job_ended_after_resume')
                self.assertEqual(check['status'], 'FAIL')
                self.assertIn('"job_terminal": false', check['reason'])

    def test_recorded_never_attempted_quota_stop_reason_names_boat_progress(self):
        from probe import SandboxWaitTimeout
        case = self.recorded()['large_quota_stop']
        probe = Mock()
        probe.data = {}
        probe.quota_stop_wait_s = 1800
        probe.request.return_value = {'operation': case['stop_operation']}
        def wait(sid, states, seconds, *, observation, on_sample):
            observation.update(case['wait'], samples=case['samples'])
            raise SandboxWaitTimeout(sid, states, case['wait']['seconds'], case['wait']['polls'],
                                     case['samples'][-1]['sandbox'], observation)
        probe.wait_sandbox.side_effect = wait
        runner = host.RunnerHost(probe)
        disk = case['pre_stop_disk']
        row = {'pre_stop': {'root_free_bytes': disk['root_free_bytes'], 'xfs_free_bytes': disk['xfs_free_bytes'],
                            'allocated_bytes': disk['retained_allocated_bytes'], 'job_running': True},
               'pre_stop_storage': {'backing_file': disk['backing_file'],
                                    'backing_allocated_blocks': disk['backing_allocated_bytes'] // 512}}
        with self.assertRaisesRegex(ValueError, 'boat_stop_never_attempted: Boat accepted stop stop_81053a3051114291'):
            runner.stop_at_quota_limit(case['sandbox_id'], {}, row)
        failure = next(r for r in runner.data['checks'] if r['name'] == 'disk_quota.stop_with_quota_sized_target')
        self.assertTrue(failure['reason'].startswith(
            'stop at quota limit did not complete: boat_stop_never_attempted: Boat accepted stop stop_81053a3051114291 '
            'at 2026-10-25T04:19:52.690Z and never attempted it'))
        for value in ('2026-10-25T04:29:27.847Z', str(disk['root_free_bytes']), str(disk['xfs_free_bytes'])):
            self.assertIn(value, failure['reason'])
        self.assertEqual(failure['evidence']['boat_stop_progress']['classification'], 'boat_stop_never_attempted')

    def test_recorded_refused_resumed_jobs_name_the_runner_refusal(self):
        parsed = self.recorded()['default_resume']['job_toolchain_parity_result']
        passed, evidence, reason = host.parity_job_check(parsed)
        self.assertFalse(passed)
        self.assertIs(evidence, parsed)
        self.assertIn('runner refused the job before start: runner_disk_full: free_bytes=25835020288 '
                      'floor_bytes=34426847232', reason)
        lane = host.RunnerHost(Mock(data={}))
        lane.check_job('resumed.node_modules_first', parsed, 0, marker='boat-node-modules')
        self.assertIn('runner_disk_full: free_bytes=25835020288', lane.data['checks'][0]['reason'])
        phase = __import__('inspect').getsource(host.RunnerHost._phase)
        self.assertIn('parity_job_check(parsed)', phase)

    def test_recorded_sandbox_account_pnpm_tree_is_a_named_root_path_failure(self):
        case = self.recorded()['root_owned']
        recorded = case['default_prepared_clone']['report']
        # Every recorded offender is uid 1000 under pnpm; the report kept 20 of 922.
        offenders = [line.replace('/usr/local/lib/node_modules/pnpm/', f'/usr/local/lib/node_modules/pnpm/{n}/', 1)
                     for n in range(47) for line in recorded['offenders']][:recorded['offender_count']]
        with tempfile.TemporaryDirectory() as tmp:
            checker = Path(tmp, 'root_paths.py')
            checker.write_text('BOAT_ROOTS = ("/usr/local",)\nBOAT_PROVIDER_RESTORED = {}\nREPORTED = 20\n'
                               f'OFFENDERS = {offenders!r}\n'
                               'def check(roots, provider_files=None, limit=REPORTED):\n'
                               '    return {"roots": list(roots), "entries": 18527, "offender_count": len(OFFENDERS),\n'
                               '            "offenders": OFFENDERS[:limit], "missing": [], "limit": limit}\n')
            with patch.object(host, 'ROOT_PATHS_SCRIPT', checker):
                source = host.root_paths_source()
            with contextlib.redirect_stdout(io.StringIO()) as output:
                exec(source, {})
        report = json.loads(output.getvalue())
        self.assertIsNone(report['limit'])
        self.assertEqual(len(report['offenders']), 20)
        self.assertEqual(report['offender_groups'],
                         [{'directory': '/usr/local/lib/node_modules/pnpm', 'owner': 'uid=1000', 'count': 922}])
        self.assertFalse(host.root_paths_passed(report))
        rewalk = dict(report, entries=18527)
        reason = host.root_paths_reason(report, case['default_prepared_clone']['kernel_release'], rewalk, 30)
        for text in ('boat_sandbox_account_owns_root_paths: 922 path(s)', "uid 1000, Boat's sandbox account",
                     "('/usr/local/lib/node_modules/pnpm', 'uid=1000', 922)", 'guest kernel 7.0.0-38-generic',
                     'a re-walk 30 s later found 922 offender(s) in 18527 entries (first walk: 18527 entries)'):
            self.assertIn(text, reason)
        # The same image on the 6.8 kernel sandboxes walked 937 fewer entries, all root's.
        for clean in ('default_resumed', 'large_prepared_clone'):
            self.assertEqual(case[clean]['kernel_release'], '6.8.0-138-generic')
            self.assertEqual((case[clean]['report']['entries'], case[clean]['report']['offender_count']), (17590, 0))
            self.assertTrue(host.root_paths_passed(case[clean]['report']))
        mode = dict(report, offender_groups=[{'directory': '/usr/local/bin', 'owner': 'mode', 'count': 1}])
        self.assertNotIn('boat_sandbox_account', host.root_paths_reason(mode))
        phase = __import__('inspect').getsource(host.RunnerHost._phase)
        self.assertIn('root_paths_reason(', phase)
        self.assertIn("record['root_owned_paths_rewalk']", phase)


if __name__ == "__main__":
    unittest.main()
