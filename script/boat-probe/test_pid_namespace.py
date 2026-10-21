import copy
import errno
import json
from pathlib import Path
import shlex
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
try:
    import pid_namespace as pid
finally:
    sys.path.pop(0)


class PidNamespaceTests(unittest.TestCase):
    def fixture(self):
        fixture = json.loads(Path(__file__).with_name('fixtures').joinpath('pid-namespace.json').read_text())
        job = {'uid':997, 'namespaces':{'pid':'pid:[2]'}, 'pid_namespace':fixture['pid_namespace'],
               'process_visibility':{'protected_pids':[1, 101, 102]}}
        host = {'namespaces':{'pid':'pid:[1]'}, 'systemd_version':255, 'job_security_reference':fixture['reference']}
        return job, host

    def test_pid_checks_reject_old_boat_layout(self):
        job, host = self.fixture()
        self.assertTrue(all(pid.pid_checks(job, host).values()))
        job['namespaces']['pid'] = host['namespaces']['pid']
        evidence = job['pid_namespace']
        evidence['init'] = {'cgroup_errno':errno.ENOENT, 'comm_errno':errno.ENOENT, 'status_errno':errno.ENOENT}
        evidence['pid'], evidence['ppid'] = 1025, 1024
        for signal in evidence['signals']:
            signal['errno'] = errno.EPERM
            signal['allowed'] = False
            signal.pop('target', None)
            signal['target_errno'] = errno.ENOENT
        checks = pid.pid_checks(job, host)
        self.assertEqual({name for name, passed in checks.items() if not passed},
                         {'boat_pid_profile', 'pid1_cgroup_readable', 'pid1_same_cgroup', 'pid1_is_wrapper',
                          'job_pid_is_small', 'outside_pids_esrch_or_proven_alias'})

    def test_each_isolation_check_rejects_its_broken_evidence(self):
        job, host = self.fixture()
        def mutate(path, value):
            bad = copy.deepcopy(job)
            obj = bad
            for part in path[:-1]:
                obj = obj[part]
            obj[path[-1]] = value
            return bad
        cases = [
            ('pid1_cgroup_readable', ['init', 'cgroup'], ''),
            ('pid1_same_cgroup', ['init', 'cgroup'], '0::/sandbox-init\n'),
            ('pid1_is_wrapper', ['init', 'comm'], 'systemd\n'),
            ('pid1_is_wrapper', ['init', 'status'], 'PPid:\t99\n'),
            ('job_pid_is_small', ['pid'], 900),
            ('job_pid_is_small', ['ppid'], 3),
            ('outside_pids_esrch_or_proven_alias', ['signals'], []),
            ('outside_pids_esrch_or_proven_alias', ['signals'], [{'pid':101, 'allowed':False, 'errno':errno.ESRCH}]),
            ('outside_pids_esrch_or_proven_alias', ['signals', 1, 'errno'], errno.EPERM),
            ('outside_pids_esrch_or_proven_alias', ['signals', 0, 'target', 'cgroup'], '0::/different.service\n'),
            ('outside_pids_esrch_or_proven_alias', ['signals', 0, 'target', 'starttime'], 10),
            ('job_credentials_match_systemd', ['security', 'status', 'Uid'], ['0']*4),
            ('job_credentials_match_systemd', ['security', 'status', 'Groups'], ['0', '996']),
            ('all_job_capabilities_empty', ['security', 'status', 'CapAmb'], ['1']),
            ('all_job_capabilities_empty', ['security', 'status', 'CapPrm'], ['1']),
            ('job_no_new_privileges', ['security', 'status', 'NoNewPrivs'], ['0']),
            ('job_namespace_filter', ['security', 'namespace_attempts', 'pid'], {'result':0, 'errno':0}),
            ('job_namespace_filter', ['security', 'namespace_attempts'], {}),
            ('job_namespace_filter', ['security', 'status', 'Seccomp_filters'], ['1']),
            ('proc_protections_match_systemd', ['security', 'proc_submounts'], [['/proc/sys', False]]),
            ('job_signal_state_matches_systemd', ['security', 'fresh_child', 'SigIgn'], ['1000']),
            ('session_keyring_owned_by_job', ['security', 'keyring', 'uid'], 0),
            ('session_keyring_owned_by_job', ['security', 'keyring', 'gid'], 0),
            ('session_keyring_owned_by_job', ['security', 'keyring', 'id'], host['job_security_reference']['keyring']['id']),
        ]
        for name, path, value in cases:
            with self.subTest(check=name, path=path):
                self.assertFalse(pid.pid_checks(mutate(['pid_namespace', *path], value), host)[name])
        job['namespaces']['pid'] = host['namespaces']['pid']
        self.assertFalse(pid.pid_checks(job, host)['boat_pid_profile'])
        for key in ('CapInh', 'CapPrm', 'CapEff', 'CapBnd', 'CapAmb'):
            self.assertFalse(pid.pid_checks(mutate(['pid_namespace', 'security', 'status', key], ['1']), host)['all_job_capabilities_empty'])

    def test_lifecycle_status_checks_do_not_accept_a_proxy_client_success(self):
        for code in (0, 23, 137, 143):
            good = {'code':code, 'terminal':{'outcome':{'kind':'exit', 'code':code}}}
            self.assertTrue(pid.lifecycle_checks(good, code))
            self.assertFalse(pid.lifecycle_checks(dict(good, code=1), code))
            self.assertFalse(pid.lifecycle_checks(dict(good, terminal={'outcome':{'kind':'outcome_unknown'}}), code))
            self.assertFalse(pid.lifecycle_checks(dict(good, terminal={'outcome':{'kind':'exit', 'code':1}}), code))
        self.assertFalse(pid.lifecycle_checks({}, 0))

    def test_aliases_require_job_identity_and_different_host_starttime(self):
        job, host = self.fixture()
        evidence = job['pid_namespace']
        alias = evidence['signals'][0]
        self.assertTrue(pid.outside_control_passes(alias, job['uid'], evidence['self_cgroup']))
        for key, value in [('cgroup', '0::/outside.service\n'), ('uid', 0), ('starttime', alias['host']['starttime'])]:
            bad = copy.deepcopy(alias)
            bad['target'][key] = value
            self.assertFalse(pid.outside_control_passes(bad, job['uid'], evidence['self_cgroup']), key)
        visible = copy.deepcopy(evidence['signals'][1])
        visible.update(allowed=True, target=visible['host'], alias=False)
        self.assertFalse(pid.outside_control_passes(visible, job['uid'], evidence['self_cgroup']))
        absent_but_readable = copy.deepcopy(evidence['signals'][1])
        absent_but_readable['target_errno'] = errno.EACCES
        self.assertFalse(pid.outside_control_passes(absent_but_readable, job['uid'], evidence['self_cgroup']))

    def test_setsid_teardown_requires_observed_escapee_and_no_survivors(self):
        good = {'found':[111], 'setsid':True, 'kill_mode':'control-group', 'control_exit':0, 'survivors':[]}
        self.assertTrue(pid.teardown_checks(good))
        for key, value in [('found', []), ('setsid', False), ('kill_mode', 'process'),
                           ('control_exit', 1), ('survivors', [111])]:
            self.assertFalse(pid.teardown_checks(dict(good, **{key:value})), key)
        self.assertFalse(pid.teardown_checks({}))

    def test_live_sources_compile_and_reference_runs_as_systemd_job_user(self):
        compile(pid.PID_SOURCE, 'pid-job', 'exec')
        compile(pid.TEARDOWN_SOURCE, 'pid-teardown', 'exec')
        argv = shlex.split(pid.reference_command())
        self.assertIn('--property=User=motor-job', argv)
        self.assertIn('--property=CapabilityBoundingSet=', argv)
        self.assertIn('--property=RestrictNamespaces=yes', argv)
        self.assertIn('--property=ProtectKernelTunables=yes', argv)
        compile(argv[-1], 'pid-reference', 'exec')
        compile(shlex.split(pid.control_identity_command([1, 101]))[-1], 'pid-controls', 'exec')
        self.assertIn('setsid', pid.teardown_job('boat-marker'))


if __name__ == '__main__':
    unittest.main()
