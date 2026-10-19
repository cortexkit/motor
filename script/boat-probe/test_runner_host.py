import json
import importlib.util
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
                "provider_environment": {"ASCII_TOKEN": False, "BOAT_ID": False, "BOAT_CLI_TOKEN": False}}

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


if __name__ == "__main__":
    unittest.main()
