import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
try:
    spec = importlib.util.spec_from_file_location('boat_resources', HERE / 'resources.py')
    if spec is None or spec.loader is None:
        raise ImportError('cannot load Boat resource probe')
    resources = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(resources)
finally:
    sys.path.pop(0)


class ResourceTests(unittest.TestCase):
    def sample(self):
        return {'exitCode': 0, 'stdout': json.dumps({'nproc': '8', 'affinity': list(range(8)),
            'cgroup_ancestors': [{'cpu.max': '800000 100000', 'cpuset.cpus.effective': '0-7'}],
            'meminfo': 'MemTotal: 16000000 kB\nMemAvailable: 15000000 kB\n',
            'runner_idle': {'installed': False, 'bytes': None}})}

    def test_raw_limits_and_usable_memory_are_preserved(self):
        parsed = resources.parse(self.sample())
        self.assertEqual(parsed['memory_bytes'], {'MemTotal': 16384000000, 'MemAvailable': 15360000000})
        self.assertEqual(parsed['cgroup_ancestors'][0]['cpu.max'], '800000 100000')
        self.assertIsNone(parsed['runner_idle']['bytes'])

    def test_missing_truncated_or_wrong_units_are_rejected(self):
        for sample in (dict(self.sample(), stdoutTruncated=True), dict(self.sample(), exitCode=1),
                       dict(self.sample(), stdout=self.sample()['stdout'].replace('kB', 'MB'))):
            with self.subTest(sample=sample), self.assertRaises(ValueError):
                resources.parse(sample)

    def test_sequential_deletion_precedes_next_create(self):
        calls = []
        class Probe:
            def __init__(self):
                self.data = {'cleanup': []}
                self.client = object()
            def checkpoint(self):
                pass
            def execute(self, sid, command, poll_seconds):
                calls.append(('exec', sid))
                return ResourceTests().sample()
        def create(probe, size):
            calls.append(('create', size))
            return {'id': size, 'kind': 'sandbox'}, {}
        def cleanup(client, data, checkpoint):
            resource = data['resources'][0]
            calls.append(('delete', resource['id']))
            resource['deletion_state'] = {'stage': 'waiting_for_uploads'}
        with patch.object(resources, 'create', create), patch.object(resources, 'cleanup', cleanup):
            resources.run(Probe())
        self.assertEqual(calls, [('create','large'), ('exec','large'), ('delete','large'),
                                 ('create','xlarge'), ('exec','xlarge'), ('delete','xlarge')])

    def test_guest_is_valid_and_read_only(self):
        compile(resources.GUEST, 'resource-guest', 'exec')
        for forbidden in ('write_text', 'apt-get', 'KVM_CREATE_VM'):
            self.assertNotIn(forbidden, resources.GUEST)

    def test_create_keeps_noenv_for_each_size(self):
        class Probe:
            def __init__(self):
                self.data = {'pending_creates': [], 'resources': []}
                self.calls = []
            def checkpoint(self):
                pass
            def request(self, method, path, body, **kwargs):
                self.calls.append(body)
                return {'sandbox': {'id': 'bx_test'}}
            def wait_sandbox(self, sid, ready):
                return {'id': sid}
        for size in ('large', 'xlarge'):
            probe = Probe()
            resources.create(probe, size)
            self.assertEqual(probe.calls[0], {'type': size, 'ttlSeconds': 300, 'noEnv': True})


if __name__ == '__main__':
    unittest.main()
