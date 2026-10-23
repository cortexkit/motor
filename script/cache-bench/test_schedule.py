"""Deterministic scheduler, admission and statistics fixtures; no providers/builds."""
import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import bench
import schedule


class SchedulerTests(unittest.TestCase):
    def options(self, root):
        calibration = root / 'calibration.json'
        calibration.write_text('{}')
        return argparse.Namespace(output=root, calibration=calibration, image='fixture-image',
                                  server='fixture-server', key_reader='fixture-key-reader')

    def run_fake(self, args, boat=None, count=12, start=0.0):
        clock = [start]
        calls = []
        def wait(seconds):
            clock[0] += seconds
        def leg(options, directory, repositories, configs, key=None, baseline=False):
            calls.append((directory.name, [c['name'] for c in configs], baseline, key))
            return {'complete': True, 'report': None}
        with patch.object(schedule, 'HOURS', count), patch.object(schedule, 'read_key', return_value=b'private-key\n') as reader, \
                contextlib.redirect_stdout(io.StringIO()):
            schedule.run_schedule(args, [], [{'name': 'standard'}, {'name': 'hp'}], clock=lambda: clock[0],
                                  wait=wait, boat_leg=boat or leg, server_leg=leg)
        return calls, reader.call_count, json.loads((args.output / 'state.json').read_text())

    def test_alternates_both_legs_and_baseline_is_every_sixth_hour(self):
        with tempfile.TemporaryDirectory() as tmp:
            calls, reads, state = self.run_fake(self.options(Path(tmp)))
        self.assertEqual(reads, 12)
        boats = [c for c in calls if c[0] == 'boat']
        self.assertEqual([c[1] for c in boats], [['standard', 'hp'], ['hp', 'standard']] * 6)
        self.assertEqual([i + 1 for i, c in enumerate(boats) if c[2]], [6, 12])
        self.assertEqual(len(calls), 24)
        self.assertTrue(all(h['status'] == 'passed' for h in state['iterations']))

    def test_failed_iteration_is_recorded_and_server_and_next_hour_proceed(self):
        def fail(*args):
            raise RuntimeError('must not persist secret exception details')
        with tempfile.TemporaryDirectory() as tmp:
            calls, reads, state = self.run_fake(self.options(Path(tmp)), boat=fail, count=2)
        self.assertEqual(len(calls), 2)
        self.assertEqual(reads, 2)
        self.assertEqual([h['status'] for h in state['iterations']], ['failed', 'failed'])
        self.assertEqual(state['iterations'][0]['boat']['error'], 'RuntimeError')
        self.assertNotIn('secret exception', json.dumps(state))

    def test_restart_marks_interrupted_hour_failed_and_resumes_without_repeats(self):
        invoked = []
        def interrupted(args, directory, repos, configs, key, baseline):
            invoked.append(directory.parent.name)
            if len(invoked) == 3:
                raise KeyboardInterrupt()
            return {'complete': True}
        with tempfile.TemporaryDirectory() as tmp:
            args = self.options(Path(tmp))
            with self.assertRaises(KeyboardInterrupt):
                self.run_fake(args, boat=interrupted, count=6)
            calls, reads, state = self.run_fake(args, count=6)
            self.assertEqual(reads, 3)
            self.assertEqual(len(calls), 6)
            self.assertEqual([h['status'] for h in state['iterations']], ['passed', 'passed', 'failed', 'passed', 'passed', 'passed'])
            self.assertEqual(state['started_at'], 0)
            self.assertTrue(state['iterations'][5]['baseline'])

    def test_offline_hours_are_explicitly_missed_and_changed_inputs_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args = self.options(root)
            def interrupt(*args):
                raise KeyboardInterrupt()
            with self.assertRaises(KeyboardInterrupt):
                self.run_fake(args, boat=interrupt, count=4)
            calls, reads, state = self.run_fake(args, count=4, start=7201)
            self.assertEqual(state['started_at'], 0)
            self.assertEqual([h['status'] for h in state['iterations']], ['failed', 'missed', 'passed', 'passed'])
            self.assertEqual(reads, 2)
            self.assertEqual(len(calls), 4)
            with self.assertRaisesRegex(bench.BenchError, 'inputs changed'):
                schedule.plan(root, 'different', 7201)


class AdmissionTests(unittest.TestCase):
    def test_selection_uses_measured_dependency_work_and_cold_budget_with_margin(self):
        records = {'motor': dict(cold_seconds=20, dependency_unit_seconds=30),
                   'prefrontal': dict(cold_seconds=50, dependency_unit_seconds=70),
                   'broca': dict(cold_seconds=300, dependency_unit_seconds=700)}
        self.assertEqual(schedule.selection(records), 'prefrontal')
        records['prefrontal']['dependency_unit_seconds'] = 20
        self.assertIsNone(schedule.selection(records))

    def test_scheduler_requires_successful_matching_calibration(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'calibration.json'
            path.write_text(json.dumps({'complete': False, 'selected': None}))
            args = argparse.Namespace(calibration=path, image='fixture-image')
            with self.assertRaisesRegex(bench.BenchError, 'calibration is required'):
                schedule.admitted(args, [])

    def test_dependency_html_classification_uses_versions_and_rejects_ambiguity(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            (target / 'cargo-timings').mkdir()
            units = [{'name': 'shared', 'version': '1', 'duration': 3},
                     {'name': 'shared', 'version': '2', 'duration': 8}]
            (target / 'cargo-timings/cargo-timing.html').write_text('const UNIT_DATA = ' + json.dumps(units) + ';')
            metadata = {'workspace_members': ['local'], 'packages': [
                {'id': 'local', 'name': 'shared', 'version': '1'}, {'id': 'dep', 'name': 'shared', 'version': '2'}]}
            with patch.object(bench, 'TARGET', target), patch.object(bench, 'command', return_value=json.dumps(metadata)):
                self.assertEqual(bench.dependency_timings({}), [units[1]])
            metadata['packages'].append({'id': 'ambiguous', 'name': 'shared', 'version': '2'})
            with patch.object(bench, 'TARGET', target), patch.object(bench, 'command', return_value=json.dumps(metadata)):
                with self.assertRaisesRegex(bench.BenchError, 'ambiguous'):
                    bench.dependency_timings({})


class SSHLegTests(unittest.TestCase):
    def test_network_server_uses_stdin_not_argv_and_only_calls_network_probes(self):
        config = dict(name='standard', credentials='/operator/key', storage_class='STANDARD')
        fake_driver = '''
class S3:
 def __init__(self,*args): pass
def network_probes(client,prefix,gateway,checkpoint):
 assert gateway is False
 result={'runs':[],'complete':True,'prefix':prefix}
 checkpoint(result)
 return result
def result_status(result): return 'passed'
'''
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args = argparse.Namespace(output=root, server='fixture-pinned-host')
            def execute(argv, **kwargs):
                self.assertEqual(argv[:5], ['ssh', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes'])
                self.assertNotIn('SECRET_FIXTURE', str(argv))
                self.assertNotIn('AWS_SECRET_ACCESS_KEY', kwargs['env'])
                self.assertNotIn('BOAT_API_KEY', kwargs['env'])
                payload = json.loads(kwargs['input'])
                payload['driver'] = fake_driver
                with patch.object(sys, 'stdin', io.StringIO(json.dumps(payload))), contextlib.redirect_stdout(io.StringIO()) as out:
                    exec(schedule.REMOTE, {})
                return subprocess.CompletedProcess(argv, 0, out.getvalue().encode(), b'')
            with patch.object(schedule.bench, 'credentials', return_value=('https://endpoint', 'ID', 'SECRET_FIXTURE')), \
                    patch.object(schedule.subprocess, 'run', side_effect=execute):
                result = schedule.server(args, root, [dict(name='motor', prefix='cache-bench/motor/')], [config])
            self.assertTrue(result['complete'])
            text = (root / result['report']).read_text()
            self.assertNotIn('SECRET_FIXTURE', text)
            self.assertEqual(json.loads(text)['backends'][0]['result']['runs'], [])

    def test_server_timeout_keeps_last_complete_checkpoint_not_truncated_tail(self):
        partial = dict(complete=False, backends=[dict(name='standard', status='passed', result={'latency': {}})])
        output = json.dumps(partial).encode() + b'\n{"truncated":'
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args = argparse.Namespace(output=root, server='fixture')
            with patch.object(schedule.subprocess, 'run', side_effect=subprocess.TimeoutExpired('ssh', 900, output=output)):
                result = schedule.server(args, root, [], [])
            self.assertEqual(result['exit_code'], 124)
            self.assertFalse(result['complete'])
            self.assertEqual(json.loads((root / result['report']).read_text()), partial)

    def test_remote_fill_preflights_all_prefixes_before_writing_and_uses_in_memory_clients(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixed = root / 'fixed'
            fixed.mkdir()
            assets = root / 'assets'
            assets.mkdir()
            driver = '''from pathlib import Path
ROOT=Path(%r)
calls=[]
def sha(path): return 'digest'
class S3:
 def __init__(self,endpoint,access,secret,config): self.config=config
 def objects(self,prefix):
  calls.append(('list',prefix))
  return ['existing'] if self.config.get('nonempty') else []
def matrix(args,configs,clients):
 assert args.profile=='fill' and len(clients)==2
 assert len([c for c in calls if c[0]=='list'])==4
 calls.append(('fill',args.prefix))
 args.output.mkdir(parents=True)
 (args.output/'results.json').write_text('{"complete":true}')
 return True
''' % str(fixed)
            payload: dict = dict(driver=driver, server_assets=str(assets), toolchain_layout=dict(cargo_home='/unused', rustup_home='/unused'),
                           repositories=[dict(name=n, prefix='cache-bench/' + n + '/', bundle_sha256='digest') for n in ('motor', 'prefrontal')],
                           backends=[dict(config={'name': n}, credential=['https://endpoint', 'ID', 'SECRET_FIXTURE']) for n in ('standard', 'hp')])
            environment = dict(os.environ)
            with patch.object(sys, 'stdin', io.StringIO(json.dumps(payload))), patch.object(subprocess, 'run') as run, \
                    contextlib.redirect_stdout(io.StringIO()) as out, patch.dict(os.environ, environment):
                exec(schedule.REMOTE_FILL, {})
            self.assertTrue(json.loads(out.getvalue())['complete'])
            self.assertNotIn('SECRET_FIXTURE', out.getvalue())
            self.assertEqual(run.call_count, 2)
            payload['backends'][1]['config']['nonempty'] = True
            fresh = root / 'fresh-assets'
            fresh.mkdir()
            payload['server_assets'] = str(fresh)
            with patch.object(sys, 'stdin', io.StringIO(json.dumps(payload))), patch.object(subprocess, 'run') as run, \
                    patch.dict(os.environ, environment), self.assertRaisesRegex(RuntimeError, 'nonempty'):
                exec(schedule.REMOTE_FILL, {})
            run.assert_not_called()


class SummaryTests(unittest.TestCase):
    def fixture(self, root, deltas=None):
        iterations = []
        for hour in range(1, 7):
            item = {'hour': hour, 'due_at': hour * 3600, 'status': 'passed'}
            for leg, placement in (('boat', 'B'), ('server', 'A')):
                entries = []
                for repo in ('motor', 'prefrontal'):
                    for storage in ('STANDARD', 'EXPRESS_ONEZONE'):
                        offset = (deltas[hour - 1] if deltas else 10) if storage == 'EXPRESS_ONEZONE' else 0
                        records = [dict(ok=True, timeout=False, seconds=(n + offset) / 1000) for n in range(1, 101)]
                        records += [dict(ok=False, timeout=True, seconds=5), dict(ok=False, timeout=False, seconds=1)]
                        runs = [dict(name=f'remote-read-only-{i}', wall_seconds=n + (storage == 'EXPRESS_ONEZONE'),
                                     sccache=dict(hits=8, misses=2, errors=1, read_errors=0, write_errors=0))
                                for i, n in enumerate((10, 20, 30), 1)] if leg == 'boat' else []
                        if hour == 6 and leg == 'boat' and storage == 'STANDARD':
                            runs.append(dict(name='no-cache', wall_seconds=100))
                        entries.append(dict(name=storage.lower(), storage_class=storage, repository=repo, status='probe-errors',
                                            result=dict(runs=runs, class_audit={'matches_requested': True},
                                                        latency={'direct': {'records': records}})))
                path = root / f'{hour}-{leg}.json'
                path.write_text(json.dumps({'backends': entries}))
                item[leg] = {'report': path.name, 'complete': False}
            iterations.append(item)
        iterations.append(dict(hour=7, due_at=7 * 3600, status='missed'))
        (root / 'state.json').write_text(json.dumps({'iterations': iterations}))

    def test_percentiles_rates_counts_placements_repositories_and_hourly_pairs(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            data = bench.summarize_data(root)
            self.assertEqual(len(data['groups']), 8)
            group = next(g for g in data['groups'] if g['storage_class'] == 'STANDARD' and g['placement'] == 'B' and g['repository'] == 'motor')
            fetch = group['probes']['direct/full-get']
            self.assertEqual((fetch['p50'], fetch['p95'], fetch['p99']), (50, 95, 99))
            self.assertEqual((fetch['attempts'], fetch['errors'], fetch['timeouts']), (612, 12, 6))
            self.assertAlmostEqual(fetch['error_rate'], 2 / 102)
            self.assertAlmostEqual(fetch['timeout_rate'], 1 / 102)
            self.assertEqual(group['rebuild_seconds'], dict(n=18, p50=20, p95=30, p99=30))
            self.assertEqual(group['baseline_seconds'], dict(n=1, p50=100, p95=100, p99=100))
            other = next(g for g in data['groups'] if g['storage_class'] == 'EXPRESS_ONEZONE' and g['placement'] == 'B' and g['repository'] == 'motor')
            self.assertEqual(other['baseline_seconds'], group['baseline_seconds'])
            self.assertEqual((group['hits'], group['misses'], group['cache_errors']), (144, 36, 18))
            self.assertEqual(group['hours_without_report'], 1)
            self.assertEqual(len(data['paired']), 24)
            paired = next(p for p in data['paired'] if p['placement'] == 'B')
            self.assertAlmostEqual(paired['difference']['direct/full-get/p50'], 10)
            self.assertEqual(paired['difference']['rebuild/p50_seconds'], 1)
            variation = next(v for v in data['variation'] if v['metric'] == 'direct/full-get/p50')
            self.assertIn('exceeds observed', variation['statement'])
            self.assertEqual(variation['paired_hour_iqr'], 0)
            with contextlib.redirect_stdout(io.StringIO()):
                bench.main(['--summarize', str(root)])
            text = (root / 'summary.md').read_text()
            self.assertIn('| 7 |', text)
            self.assertIn('missed', text)
            self.assertNotIn('production readiness established', text)

    def test_difference_does_not_exceed_observed_variation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root, deltas=[-10, 0, 10, 20, 30, 40])
            data = bench.summarize_data(root)
            variation = next(v for v in data['variation'] if v['metric'] == 'direct/full-get/p50')
            self.assertAlmostEqual(variation['median_difference'], 10)
            self.assertAlmostEqual(variation['paired_hour_iqr'], 30)
            self.assertIn('does not exceed', variation['statement'])

    def test_missing_pair_and_empty_observations_are_not_invented(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            path = root / '1-boat.json'
            data = json.loads(path.read_text())
            data['backends'] = [e for e in data['backends'] if e['storage_class'] == 'STANDARD']
            path.write_text(json.dumps(data))
            summary = bench.summarize_data(root)
            self.assertEqual(len(summary['paired']), 22)
            self.assertEqual(bench.probe_distribution([])['error_rate'], None)
            variation = next(v for v in summary['variation'] if v['metric'] == 'rebuild/p50_seconds')
            self.assertIn('insufficient paired hours', variation['statement'])


if __name__ == '__main__':
    unittest.main()
