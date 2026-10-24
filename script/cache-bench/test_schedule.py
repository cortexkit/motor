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


MOTOR = dict(name='motor', prefix='cache-bench/motor/')
BROCA = dict(name='broca', prefix='cache-bench/broca/')
HEAVY = dict(name='prefrontal', prefix='cache-bench/prefrontal/')
# Cold Cargo seconds measured on Boat large for the OVH 72-hour experiment.
MEASURED = {'motor': dict(cold_seconds=44.95735893599999, dependency_unit_seconds=84.58000000000001),
            'prefrontal': dict(cold_seconds=197.18450059999998, dependency_unit_seconds=185.76),
            'broca': dict(cold_seconds=203.7015198460001, dependency_unit_seconds=230.06)}


class SchedulerTests(unittest.TestCase):
    def options(self, root):
        calibration = root / 'calibration.json'
        calibration.write_text('{}')
        return argparse.Namespace(output=root, calibration=calibration, image='fixture-image',
                                  server='fixture-server', key_reader='fixture-key-reader')

    def run_fake(self, args, boat=None, count=12, start=0.0, repositories=(MOTOR, BROCA), sizes=None):
        clock = [start]
        calls = []
        def wait(seconds):
            clock[0] += seconds
        def boat_leg(options, directory, repos, configs, key, baseline=False, profile='hourly', samples=None):
            calls.append(dict(leg='boat', hour=directory.parent.name, sandbox=directory.name,
                              repositories=[r['name'] for r in repos], configs=[c['name'] for c in configs],
                              baseline=baseline, profile=profile, samples=samples, key=key))
            return {'complete': True, 'report': None}
        def server_leg(options, directory, repos, configs):
            calls.append(dict(leg='server', hour=directory.name, repositories=[r['name'] for r in repos],
                              configs=[c['name'] for c in configs]))
            return {'complete': True, 'report': None}
        if sizes is None:
            sizes = {r['name']: schedule.sizing(MEASURED[r['name']]['cold_seconds']) for r in repositories}
        with patch.object(schedule, 'HOURS', count), patch.object(schedule, 'read_key', return_value=b'private-key\n') as reader, \
                contextlib.redirect_stdout(io.StringIO()):
            schedule.run_schedule(args, list(repositories), [{'name': 'standard'}, {'name': 'hp'}], sizes,
                                  clock=lambda: clock[0], wait=wait, boat_leg=boat or boat_leg, server_leg=server_leg)
        return calls, reader.call_count, json.loads((args.output / 'state.json').read_text())

    def test_each_repository_gets_its_own_sandbox_with_its_own_samples_and_class_alternation(self):
        with tempfile.TemporaryDirectory() as tmp:
            calls, reads, state = self.run_fake(self.options(Path(tmp)))
        boats = [c for c in calls if c['leg'] == 'boat']
        # Two sandboxes an hour, one per repository, each with a fresh key read.
        self.assertEqual(reads, 24)
        self.assertEqual(len(boats), 24)
        self.assertEqual([c['sandbox'] for c in boats], ['boat-motor', 'boat-broca'] * 12)
        self.assertEqual([c['repositories'] for c in boats], [['motor'], ['broca']] * 12)
        self.assertEqual([c['samples'] for c in boats], [3, 2] * 12)
        self.assertEqual([c['configs'] for c in boats], [['standard', 'hp']] * 2 + [['hp', 'standard']] * 2 + (
            [['standard', 'hp']] * 2 + [['hp', 'standard']] * 2) * 5)
        # broca's baseline fits beside R=2, so both baselines run inline in the sixth-hour sandboxes.
        self.assertEqual([(c['hour'], c['sandbox']) for c in boats if c['baseline']],
                         [('hour-006', 'boat-motor'), ('hour-006', 'boat-broca'),
                          ('hour-012', 'boat-motor'), ('hour-012', 'boat-broca')])
        self.assertNotIn('baseline', {c['profile'] for c in boats})
        servers = [c for c in calls if c['leg'] == 'server']
        self.assertEqual(len(servers), 12)
        self.assertTrue(all(c['repositories'] == ['motor', 'broca'] for c in servers))
        self.assertTrue(all(h['status'] == 'passed' for h in state['iterations']))
        self.assertEqual(state['sizing']['broca']['samples'], 2)
        for item in state['iterations']:
            self.assertEqual(item['samples'], {'motor': 3, 'broca': 2})
            self.assertEqual(list(item['boat']), ['motor', 'broca'])
            self.assertEqual(item['boat']['broca']['samples'], 2)

    def test_baseline_that_does_not_fit_runs_in_its_own_sandbox_and_is_never_skipped(self):
        # 300 s cold: R=2 plus a baseline needs 5 x 300 + 540 = 2040 s, so the baseline moves out.
        sizes = {'motor': schedule.sizing(45), 'prefrontal': schedule.sizing(300)}
        self.assertEqual(sizes['prefrontal']['samples'], 2)
        self.assertTrue(sizes['prefrontal']['baseline_sandbox'])
        with tempfile.TemporaryDirectory() as tmp:
            calls, reads, state = self.run_fake(self.options(Path(tmp)), repositories=(MOTOR, HEAVY), sizes=sizes)
        boats = [c for c in calls if c['leg'] == 'boat']
        sixth = [c for c in boats if c['hour'] == 'hour-006']
        self.assertEqual([c['sandbox'] for c in sixth], ['boat-motor', 'boat-prefrontal-baseline', 'boat-prefrontal'])
        motor, alone, heavy = sixth
        self.assertTrue(motor['baseline'])
        self.assertEqual((motor['samples'], motor['profile']), (3, 'hourly'))
        self.assertEqual((alone['profile'], alone['samples'], alone['configs'], alone['repositories']),
                         ('baseline', None, ['standard'], ['prefrontal']))
        self.assertEqual((heavy['profile'], heavy['samples'], heavy['baseline']), ('hourly', 2, False))
        self.assertEqual([c['hour'] for c in boats if c['profile'] == 'baseline'], ['hour-006', 'hour-012'])
        self.assertEqual(len(boats), 26)
        self.assertEqual(reads, 26)
        self.assertEqual(list(state['iterations'][5]['boat']), ['motor', 'prefrontal-baseline', 'prefrontal'])
        self.assertEqual(state['iterations'][5]['boat']['prefrontal-baseline']['profile'], 'baseline')
        self.assertEqual(list(state['iterations'][4]['boat']), ['motor', 'prefrontal'])

    def test_failed_sandbox_is_recorded_and_other_sandbox_server_and_next_hour_proceed(self):
        seen = []
        def fail(options, directory, repos, configs, key, baseline=False, profile='hourly', samples=None):
            seen.append(directory.name)
            if repos[0]['name'] == 'motor':
                raise RuntimeError('must not persist secret exception details')
            return {'complete': True}
        with tempfile.TemporaryDirectory() as tmp:
            calls, reads, state = self.run_fake(self.options(Path(tmp)), boat=fail, count=2)
        self.assertEqual(seen, ['boat-motor', 'boat-broca'] * 2)
        self.assertEqual(len(calls), 2)
        self.assertEqual(reads, 4)
        self.assertEqual([h['status'] for h in state['iterations']], ['failed', 'failed'])
        self.assertEqual(state['iterations'][0]['boat']['motor']['error'], 'RuntimeError')
        self.assertTrue(state['iterations'][0]['boat']['broca']['complete'])
        self.assertTrue(state['iterations'][0]['server']['complete'])
        self.assertNotIn('secret exception', json.dumps(state))

    def test_unresolved_cleanup_of_a_previous_sandbox_blocks_the_next_sandbox(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            lifecycle = root / 'hour-001/boat-broca/probe/results.json'
            lifecycle.parent.mkdir(parents=True)
            lifecycle.write_text(json.dumps({'cleanup_complete': False}))
            with patch.object(schedule, 'cleanup', return_value=False) as cleanup:
                calls, reads, state = self.run_fake(self.options(root), count=1)
        self.assertEqual([c['leg'] for c in calls], ['server'])
        self.assertEqual(cleanup.call_count, 2)
        self.assertEqual(state['iterations'][0]['boat']['broca']['error'], 'BenchError')

    def test_restart_marks_interrupted_hour_failed_and_resumes_without_repeats(self):
        invoked = []
        def interrupted(options, directory, repos, configs, key, baseline=False, profile='hourly', samples=None):
            invoked.append((directory.parent.name, directory.name, samples))
            # Interrupt the scheduler inside hour 3's second (broca) sandbox.
            if len(invoked) == 6:
                raise KeyboardInterrupt()
            return {'complete': True}
        with tempfile.TemporaryDirectory() as tmp:
            args = self.options(Path(tmp))
            with self.assertRaises(KeyboardInterrupt):
                self.run_fake(args, boat=interrupted, count=6)
            first = json.loads((args.output / 'state.json').read_text())
            self.assertEqual(first['iterations'][2]['status'], 'running')
            self.assertEqual(list(first['iterations'][2]['boat']), ['motor'])
            calls, reads, state = self.run_fake(args, count=6)
            self.assertEqual(invoked[-1], ('hour-003', 'boat-broca', 2))
            boats = [c for c in calls if c['leg'] == 'boat']
            self.assertEqual([c['hour'] for c in boats], ['hour-004', 'hour-004', 'hour-005', 'hour-005', 'hour-006', 'hour-006'])
            self.assertEqual([c['samples'] for c in boats], [3, 2] * 3)
            self.assertEqual(reads, 6)
            self.assertEqual([h['status'] for h in state['iterations']], ['passed', 'passed', 'failed', 'passed', 'passed', 'passed'])
            self.assertEqual(state['iterations'][2]['error'], 'scheduler interrupted; sample not repeated')
            # The interrupted hour keeps the sandbox evidence it finished before the interruption.
            self.assertTrue(state['iterations'][2]['boat']['motor']['complete'])
            self.assertEqual(state['started_at'], 0)
            self.assertTrue(state['iterations'][5]['baseline'])
            self.assertEqual(state['iterations'][5]['samples'], {'motor': 3, 'broca': 2})
            # Resuming with a different per-repository sample plan is a changed input.
            changed = {'motor': schedule.sizing(45), 'broca': dict(schedule.sizing(204), samples=3)}
            with self.assertRaisesRegex(bench.BenchError, 'inputs changed'):
                self.run_fake(args, count=6, sizes=changed)

    def test_offline_hours_are_explicitly_missed_and_changed_inputs_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args = self.options(root)
            def interrupt(*args, **kwargs):
                raise KeyboardInterrupt()
            with self.assertRaises(KeyboardInterrupt):
                self.run_fake(args, boat=interrupt, count=4)
            calls, reads, state = self.run_fake(args, count=4, start=7201)
            self.assertEqual(state['started_at'], 0)
            self.assertEqual([h['status'] for h in state['iterations']], ['failed', 'missed', 'passed', 'passed'])
            self.assertEqual(reads, 4)
            self.assertEqual(len(calls), 6)
            with self.assertRaisesRegex(bench.BenchError, 'inputs changed'):
                schedule.plan(root, 'different', 7201)


class AdmissionTests(unittest.TestCase):
    def test_per_repository_budget_chooses_the_largest_sample_count_that_fits(self):
        motor = schedule.sizing(MEASURED['motor']['cold_seconds'])
        self.assertEqual((motor['samples'], motor['baseline_sandbox']), (3, False))
        self.assertAlmostEqual(motor['baseline_hour_budget_seconds'], 7 * 44.95735893599999 + 540)
        broca = schedule.sizing(MEASURED['broca']['cold_seconds'])
        # R=3 with the sixth-hour baseline needs 7 x 203.7 + 540 = 1966 s, over the 1,800 s TTL.
        self.assertEqual((broca['samples'], broca['baseline_sandbox']), (2, False))
        self.assertAlmostEqual(broca['baseline_hour_budget_seconds'], 5 * 203.7015198460001 + 540)
        self.assertEqual(schedule.sizing(180)['samples'], 3)
        self.assertEqual(schedule.sizing(180.01)['samples'], 2)
        self.assertFalse(schedule.sizing(252)['baseline_sandbox'])
        separate = schedule.sizing(252.01)
        self.assertEqual((separate['samples'], separate['baseline_sandbox']), (2, True))
        self.assertAlmostEqual(separate['baseline_sandbox_budget_seconds'], 252.01 + 540)
        self.assertTrue(schedule.sizing(315)['baseline_sandbox'])
        self.assertIsNone(schedule.sizing(315.01))
        self.assertIsNone(schedule.sizing(1800))

    def test_selection_prefers_more_dependency_work_then_falls_back_then_refuses(self):
        records = {name: dict(value) for name, value in MEASURED.items()}
        self.assertEqual(schedule.selection(records), 'broca')
        sizes = schedule.scheduled_sizing(records, 'broca')
        self.assertEqual(list(sizes), ['motor', 'broca'])
        self.assertEqual((sizes['motor']['samples'], sizes['broca']['samples']), (3, 2))
        records['broca']['cold_seconds'] = 400
        self.assertEqual(schedule.selection(records), 'prefrontal')
        records['prefrontal']['cold_seconds'] = 316
        self.assertIsNone(schedule.selection(records))
        # A light candidate never qualifies, even when it fits easily.
        records = {name: dict(value) for name, value in MEASURED.items()}
        records['broca']['dependency_unit_seconds'] = 20
        self.assertEqual(schedule.selection(records), 'prefrontal')
        records['prefrontal']['dependency_unit_seconds'] = 20
        self.assertIsNone(schedule.selection(records))

    def calibration_fixture(self, root, cold):
        # Fake Boat calibration sandboxes write the same raw report shape the real probe saves.
        repositories = []
        for name in ('motor', 'prefrontal', 'broca'):
            repositories.append(dict(name=name, bundle=str(root / (name + '.tar.gz')), bundle_sha256=name[0] * 64,
                                     prefix='cache-bench/fixture-' + name + '/'))
        args = argparse.Namespace(output=root / 'calibration', image='fixture-image', key_reader='fixture-key-reader')
        args.output.mkdir()
        def boat_leg(options, directory, repos, configs, key, baseline=False, profile='hourly', samples=None):
            self.assertEqual((profile, samples, len(configs)), ('baseline', None, 1))
            name = repos[0]['name']
            report = directory / 'probe/cache-results/results.json'
            report.parent.mkdir(parents=True)
            steps = [dict(dependency_unit_seconds=cold[name][1] / 2)] * 2
            report.write_text(json.dumps({'complete': True, 'backends': [{'result': {'runs': [
                dict(name='no-cache', wall_seconds=cold[name][0], steps=steps)]}}]}))
            return {'exit_code': 0, 'report': str(report.relative_to(args.output)), 'complete': True}
        with patch.object(schedule, 'boat', side_effect=boat_leg), \
                patch.object(schedule, 'read_key', return_value=b'private-key\n'), \
                contextlib.redirect_stdout(io.StringIO()):
            selected = schedule.calibrate(args, repositories, [{'name': 'standard'}, {'name': 'hp'}])
        return selected, repositories, args.output / 'calibration.json'

    def test_calibration_records_sample_counts_and_the_scheduler_admits_only_that_plan(self):
        cold = {name: (value['cold_seconds'], value['dependency_unit_seconds']) for name, value in MEASURED.items()}
        with tempfile.TemporaryDirectory() as tmp:
            selected, repositories, path = self.calibration_fixture(Path(tmp), cold)
            self.assertTrue(selected)
            evidence = json.loads(path.read_text())
            self.assertEqual(evidence['schema'], 2)
            self.assertEqual(evidence['selected'], 'broca')
            self.assertEqual({name: record['sizing']['samples'] for name, record in evidence['records'].items()},
                             {'motor': 3, 'prefrontal': 2, 'broca': 2})
            self.assertEqual({name: value['samples'] for name, value in evidence['sizing'].items()}, {'motor': 3, 'broca': 2})
            self.assertAlmostEqual(evidence['records']['broca']['dependency_unit_seconds'], 230.06)
            args = argparse.Namespace(calibration=path, image='fixture-image')
            chosen, sizes = schedule.admitted(args, list(reversed(repositories)))
            self.assertEqual([r['name'] for r in chosen], ['motor', 'broca'])
            self.assertEqual(sizes, evidence['sizing'])
            # A hand-edited sample count is not the plan the measurements support.
            evidence['sizing']['broca']['samples'] = 3
            path.write_text(json.dumps(evidence))
            with self.assertRaisesRegex(bench.BenchError, 'calibration is required'):
                schedule.admitted(args, repositories)
            evidence = json.loads(path.read_text())
            evidence['sizing']['broca']['samples'] = 2
            path.write_text(json.dumps(evidence))
            schedule.admitted(args, repositories)
            # A bundle or prefix that differs from calibration is refused.
            moved = [dict(r, prefix=r['prefix'].replace('fixture', 'other')) for r in repositories]
            with self.assertRaisesRegex(bench.BenchError, 'recalibrate'):
                schedule.admitted(args, moved)

    def test_calibration_refuses_when_no_candidate_fits_its_own_sandbox(self):
        cold = {'motor': (45, 85), 'prefrontal': (320, 186), 'broca': (400, 230)}
        with tempfile.TemporaryDirectory() as tmp:
            selected, repositories, path = self.calibration_fixture(Path(tmp), cold)
            self.assertFalse(selected)
            evidence = json.loads(path.read_text())
            self.assertIsNone(evidence['selected'])
            self.assertIsNone(evidence['records']['broca']['sizing'])
            with self.assertRaisesRegex(bench.BenchError, 'calibration is required'):
                schedule.admitted(argparse.Namespace(calibration=path, image='fixture-image'), repositories)

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
    SAMPLES = {'motor': 3, 'prefrontal': 2}

    def report(self, root, name, repos, boat, hour, deltas):
        entries = []
        for repo in repos:
            for storage in ('STANDARD', 'EXPRESS_ONEZONE'):
                offset = (deltas[hour - 1] if deltas else 10) if storage == 'EXPRESS_ONEZONE' else 0
                records = [dict(ok=True, timeout=False, seconds=(n + offset) / 1000) for n in range(1, 101)]
                records += [dict(ok=False, timeout=True, seconds=5), dict(ok=False, timeout=False, seconds=1)]
                runs = [dict(name=f'remote-read-only-{i}', wall_seconds=n + (storage == 'EXPRESS_ONEZONE'),
                             sccache=dict(hits=8, misses=2, errors=1, read_errors=0, write_errors=0))
                        for i, n in enumerate((10, 20, 30)[:self.SAMPLES[repo]], 1)] if boat else []
                if hour == 6 and boat and storage == 'STANDARD' and repo == 'motor':
                    runs.append(dict(name='no-cache', wall_seconds=100))
                entries.append(dict(name=storage.lower(), storage_class=storage, repository=repo, status='probe-errors',
                                    result=dict(runs=runs, class_audit={'matches_requested': True},
                                                latency={'direct': {'records': records}})))
        path = root / f'{hour}-{name}.json'
        path.write_text(json.dumps({'backends': entries}))
        return path.name

    def fixture(self, root, deltas=None):
        """Six slots of the per-repository layout: motor at R=3, prefrontal at R=2 with a separate baseline."""
        iterations = []
        for hour in range(1, 7):
            item = {'hour': hour, 'due_at': hour * 3600, 'status': 'passed', 'samples': dict(self.SAMPLES), 'boat': {}}
            for repo in ('motor', 'prefrontal'):
                if hour == 6 and repo == 'prefrontal':
                    path = root / '6-boat-prefrontal-baseline.json'
                    path.write_text(json.dumps({'backends': [dict(name='standard', storage_class='STANDARD', repository=repo,
                                                                  status='passed', result=dict(runs=[
                                                                      dict(name='no-cache', wall_seconds=200)]))]}))
                    item['boat']['prefrontal-baseline'] = {'report': path.name, 'complete': True, 'repository': repo,
                                                           'profile': 'baseline', 'samples': None}
                item['boat'][repo] = {'report': self.report(root, 'boat-' + repo, [repo], True, hour, deltas),
                                      'complete': False, 'repository': repo, 'profile': 'hourly',
                                      'samples': self.SAMPLES[repo]}
            item['server'] = {'report': self.report(root, 'server', ['motor', 'prefrontal'], False, hour, deltas),
                              'complete': False}
            iterations.append(item)
        iterations.append(dict(hour=7, due_at=7 * 3600, status='missed'))
        sizing = {'motor': dict(samples=3, baseline_sandbox=False), 'prefrontal': dict(samples=2, baseline_sandbox=True)}
        (root / 'state.json').write_text(json.dumps({'sizing': sizing, 'iterations': iterations}))

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
            self.assertEqual(group['read_only_samples_per_class'], [3])
            other = next(g for g in data['groups'] if g['storage_class'] == 'EXPRESS_ONEZONE' and g['placement'] == 'B' and g['repository'] == 'motor')
            self.assertEqual(other['baseline_seconds'], group['baseline_seconds'])
            self.assertEqual((group['hits'], group['misses'], group['cache_errors']), (144, 36, 18))
            self.assertEqual(group['hours_without_report'], 1)
            # prefrontal: two samples per class per hour, and its separate baseline sandbox
            # adds only to the baseline pool, not to observed hours or class rows.
            heavy = next(g for g in data['groups'] if g['storage_class'] == 'STANDARD' and g['placement'] == 'B' and g['repository'] == 'prefrontal')
            self.assertEqual(heavy['rebuild_seconds'], dict(n=12, p50=10, p95=20, p99=20))
            self.assertEqual(heavy['baseline_seconds'], dict(n=1, p50=200, p95=200, p99=200))
            self.assertEqual((heavy['hours'], heavy['read_only_samples_per_class']), (6, [2]))
            self.assertEqual(data['repositories']['prefrontal'], dict(samples=2, baseline_sandbox=True))
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
            self.assertIn('## Repository motor', text)
            self.assertIn('R = 3 Boat read-only samples per class per hour.', text)
            self.assertIn('## Repository prefrontal', text)
            self.assertIn('R = 2 Boat read-only samples per class per hour. Its sixth-hour baseline runs in a separate', text)
            self.assertIn('### Class comparison for prefrontal (HP minus Standard, R = 2)', text)
            self.assertLess(text.index('## Repository motor'), text.index('## Repository prefrontal'))
            self.assertIn('motor: False, prefrontal-baseline: True, prefrontal: False', text)
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
            for name in ('motor', 'prefrontal'):
                path = root / f'1-boat-{name}.json'
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
