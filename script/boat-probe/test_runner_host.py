import json
import importlib.util
import contextlib
import io
from unittest.mock import patch, Mock
from pathlib import Path
import sys
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
    def test_client_requires_a_terminal(self):
        self.assertEqual(host.job_result('{"code":23,"terminal":{"outcome":{"kind":"exit","code":23}}}')["code"], 23)
        for text in ("{}", "not json", '{"code":true,"terminal":{}}'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                host.job_result(text)

    def fixture(self):
        return {"user": "motor-job", "uid": 997, "interfaces": ["lo"],
                "namespaces": {"net": "net:[2]", "pid": "pid:[1]", "mnt": "mnt:[2]"},
                "private_shm": True, "shm_writable": True, "nproc": "8",
                "limits": {"memory.max": "15569256448", "cpu.max": "800000 100000", "pids.max": "4096"},
                "provider_environment": {"ASCII_TOKEN": False, "BOAT_ID": False, "BOAT_CLI_TOKEN": False},
                "status_security": {'NoNewPrivs':'1','CapEff':'0000000000000000','CapBnd':'0000000000000000'},
                "proc_mountinfo": ['proc /proc rw - proc proc rw,hidepid=invisible,subset=pid'],
                "process_visibility": {'self_pid':10,'protected_pids':[1,2], 'proc':[{'pid':10,'uid':997,'same_cgroup':True}],
                    'ps':{'exit_code':0,'rows':[{'pid':10,'uid':997,'comm':'python3'}]},
                    'runner_signal_probe':{'pid':1,'allowed':False,'errno':1}},
                "provider_access": {'inherited_names': [], 'files': {'/home/user/.config/ascii/boat/config.json': {'readable':False}},
                                    'processes': {'1': {'readable':False}}}}

    def test_real_job_limit_and_namespace_parser(self):
        job = self.fixture()
        baseline = {"namespaces": {"net": "net:[1]", "pid": "pid:[1]"},
                    "memory_budget": {"mem_available_bytes": 16*(1<<30), "runner_idle_bytes": 1<<30,
                                      "margin_bytes": 512*(1<<20), "memory_max_bytes": 15569256448}}
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
        self.assertNotEqual(host.PORT, 9443)

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

    def test_failure_diagnostics_capture_each_unit_even_when_status_is_nonzero(self):
        output = io.StringIO()
        def command(argv, **kwargs):
            return Mock(returncode=3 if 'status' in argv else 0, stdout='captured '+argv[-1], stderr='')
        with patch.object(host.subprocess, 'run', command), contextlib.redirect_stdout(output):
            exec(host.DIAGNOSTICS, {})
        data = json.loads(output.getvalue())
        self.assertEqual(set(data), {'motor-boat.service','motor-boat-host.service','motor-boat-host.path','motor-boat-bridge.service'})
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


if __name__ == "__main__":
    unittest.main()
