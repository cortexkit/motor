import errno
import contextlib
import io
import json
import os
import pathlib
import tempfile
import unittest
from unittest.mock import Mock, patch

import disk_quota
import quota_adversarial


class DiskQuotaChecks(unittest.TestCase):
    def test_only_retained_job_deadline_is_rounded_up_and_capped_by_phase_lifetime(self):
        for remaining, expected in ((600, 600), (600.1, 601), (2000, 1800), (0.1, 1)):
            self.assertEqual(disk_quota.job_seconds('retained_target', remaining), expected)
        for lane in disk_quota.LANES:
            self.assertEqual(disk_quota.job_seconds(lane, 600), 900)

    def test_retained_hold_requires_live_allocated_samples_not_only_completed_stop(self):
        initial = {'job_running': True, 'allocated_bytes': 2**30}
        live = {'sandbox': {'state': 'archiving'},
                'retained_job': {'status': 'observed', 'job_running': True, 'retained_target_at_limit': True}}
        archived = {'sandbox': {'state': 'archived', 'stop': {'status': 'completed'}}}
        self.assertEqual(disk_quota.retained_hold_status([live, archived]), 'PASS')
        self.assertEqual(disk_quota.retained_hold_status([archived], initial), 'PASS')
        self.assertEqual(disk_quota.retained_hold_status([archived]), 'INCONCLUSIVE')
        for change in ({'job_running': False}, {'retained_target_at_limit': False}):
            failed = dict(live, retained_job=dict(live['retained_job'], **change))
            self.assertEqual(disk_quota.retained_hold_status([live, failed, archived], initial), 'FAIL')
        for guest in ({'status': 'unavailable'}, {}, {'status': 'observed', 'job_running': None}):
            gap = dict(live, retained_job=guest)
            self.assertEqual(disk_quota.retained_hold_status([live, gap, archived], initial), 'INCONCLUSIVE')

    def test_retained_sample_reads_job_liveness_and_actual_file_allocation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            job_id, unit = '00000000-0000-4000-8000-000000000001', '00000000-0000-4000-8000-000000000002'
            record = root / 'state/jobs' / job_id / 'record.json'
            record.parent.mkdir(parents=True)
            record.write_text(json.dumps({'started_ms': 1, 'terminal': None, 'process': {'unit': unit}}))
            fill = root / 'retained-fill'
            fill.write_bytes(b'x' * (2 * 1024**2))
            source = quota_adversarial.retained_sample({'job_id': job_id, 'host_fill_path': str(fill),
                                                       'lease': {'growth_bytes': 18 * 1024**2}})
            # The concrete class: Python 3.11's Path.__new__ compares cls against
            # the module's Path, which this test patches, and then fails.
            real_path, statvfs = pathlib.PosixPath, os.statvfs
            def path(value):
                return root / 'state' if value == '/var/lib/motor-boat/state/boat' else real_path(value)
            def sample(active):
                output = io.StringIO()
                with patch.object(pathlib, 'Path', side_effect=path), \
                     patch.object(os, 'statvfs', side_effect=lambda _: statvfs(directory)), \
                     patch('subprocess.run', return_value=Mock(returncode=0 if active else 3)), \
                     contextlib.redirect_stdout(output):
                    exec(compile(source, '<retained-sample>', 'exec'), {})
                return json.loads(output.getvalue())
            observed = sample(True)
            self.assertTrue(observed['job_running'])
            self.assertTrue(observed['retained_target_at_limit'])
            self.assertEqual(observed['allocated_bytes'], 2 * 1024**2)
            self.assertFalse(sample(False)['job_running'])
            fill.unlink()
            observed = sample(True)
            self.assertEqual(observed['allocated_bytes'], 0)
            self.assertFalse(observed['retained_target_at_limit'])

    def test_retained_hold_can_cover_the_remaining_phase_without_changing_other_lane_waits(self):
        source = disk_quota.job('retained_target', 'token', hold_seconds=1700)
        self.assertIn('QUOTA_HOLD_SECONDS=1700', source)
        self.assertIn("acknowledged(QUOTA_HOLD_SECONDS if QUOTA_LANE == 'retained_target' else 90)", source)
        self.assertIn('assert acknowledged()', source)
        compile(source, '<long-held-job>', 'exec')

    def test_resume_requires_retained_workspace_eviction_and_ready_capacity(self):
        before = dict(retained_allocated_bytes=25_501_368_320, lease=dict(roots=['/Users/repo']))
        after = dict(workspace='/Users/repo', workspace_absent=True, eviction_logged=True,
                     root_free_bytes=60 << 30, xfs_free_bytes=60 << 30)
        self.assertTrue(all(disk_quota.resume_eviction_checks(before, after, True).values()))
        for change in ({'workspace_absent': False}, {'eviction_logged': False}, {'workspace': '/Users/other'},
                       {'root_free_bytes': 33_494_822_912}, {'xfs_free_bytes': 33_494_822_912}):
            with self.subTest(change=change):
                self.assertFalse(all(disk_quota.resume_eviction_checks(before, dict(after, **change), True).values()))
        self.assertFalse(all(disk_quota.resume_eviction_checks({}, after, True).values()))
        self.assertFalse(all(disk_quota.resume_eviction_checks(before, after, False).values()))

    def test_resume_journal_requires_success_for_exact_workspace_after_cursor(self):
        source = quota_adversarial.resume_eviction_source('/Users/repo', 'saved-cursor')
        for message, passed in (
            ('runner disk eviction path=/Users/repo bytes=25501368320', True),
            ('runner disk eviction failed path=/Users/repo error=EXDEV', False),
            ('runner disk eviction path=/Users/repo-other bytes=25501368320', False),
            ('runner disk eviction path=/Users/other bytes=25501368320', False),
        ):
            with self.subTest(message=message), contextlib.redirect_stdout(io.StringIO()) as output, \
                 patch('subprocess.run', return_value=Mock(stdout=json.dumps({'MESSAGE': message}))) as run, \
                 patch('os.path.lexists', return_value=False), \
                 patch('os.statvfs', return_value=Mock(f_bavail=60 << 30, f_frsize=1)):
                exec(compile(source, '<resume-eviction>', 'exec'), {})
                self.assertEqual(json.loads(output.getvalue())['eviction_logged'], passed)
                self.assertIn('--after-cursor=saved-cursor', run.call_args.args[0])

    def test_backing_allocation_requires_a_real_drop_on_the_expected_image(self):
        # The drop must cover the evicted retained target's allocation (less
        # RECLAIM_SLACK_BYTES); a shrink of a few sectors is not a reclaim.
        retained = 4 * 1024**3
        before = dict(expected_backing=True, backing_file='/var/lib/motor-boat.xfs',
                      backing_allocated_blocks=1000 + retained // 512)
        after = dict(before, backing_allocated_blocks=100)
        self.assertTrue(disk_quota.backing_allocation_dropped(before, after, retained))
        for change in ({'backing_allocated_blocks': before['backing_allocated_blocks']},
                       {'backing_allocated_blocks': before['backing_allocated_blocks'] + 1},
                       {'backing_allocated_blocks': before['backing_allocated_blocks'] - 900},
                       {'backing_allocated_blocks': None}, {'backing_allocated_blocks': True},
                       {'backing_allocated_blocks': -1}, {'expected_backing': False},
                       {'backing_file': '/other.xfs'}):
            with self.subTest(change=change):
                self.assertFalse(disk_quota.backing_allocation_dropped(before, dict(after, **change), retained))
        self.assertFalse(disk_quota.backing_allocation_dropped({}, after, retained))
        self.assertFalse(disk_quota.backing_allocation_dropped(before, {}, retained))

    def evidence(self):
        result = dict(code=73, stdout=json.dumps(dict(lane='tmp', errno=errno.EDQUOT,
                      projects={name: 1 for name in ('workspace', 'target', 'home', 'tmp', 'var_tmp', 'stdout', 'stderr', 'exit', 'cache-stats')},
                      inheritance=dict(project=1, child_project=1,
                      setters_refused=[0x401c5820, 0x40086602, 0x40046602], file_setattr_errno=errno.ENOSYS))))
        at_limit = dict(errno=errno.EDQUOT, bytes_written=2**30, lease=dict(blocks=2**21, project=1, growth_bytes=2**30),
                        kernel_hard_limit_warning=True,
                        kernel_usage=dict(hard_bytes=2**30, used_bytes=2**30), before=dict(physical_allowance_bytes=2**30),
                        root_free_bytes=disk_quota.RESERVE_BYTES, xfs_free_bytes=disk_quota.RESERVE_BYTES)
        cleanup = dict(ready=True, leases=[], root_free_bytes=2**36, xfs_free_bytes=2**36)
        return result, at_limit, cleanup

    def test_accepts_quota_failure_with_both_reserves_and_cleanup(self):
        self.assertTrue(all(disk_quota.checks(*self.evidence()).values()))

    def test_rejects_missing_or_wrong_errno_and_successful_job(self):
        for value in (errno.EIO, None):
            result, at_limit, cleanup = self.evidence()
            result['stdout'] = json.dumps(dict(errno=value))
            self.assertFalse(disk_quota.checks(result, at_limit, cleanup)['job_write_failed'])
        result, at_limit, cleanup = self.evidence()
        result['code'] = 0
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup)['job_write_failed'])

    def test_rejects_either_disk_losing_reserve_at_limit(self):
        for key in ('root_free_bytes', 'xfs_free_bytes'):
            result, at_limit, cleanup = self.evidence()
            at_limit[key] = disk_quota.RESERVE_BYTES - 1
            self.assertFalse(disk_quota.checks(result, at_limit, cleanup)['reserve_at_limit'])

    def test_rejects_missing_fill_or_unclean_lease(self):
        result, at_limit, cleanup = self.evidence()
        at_limit['bytes_written'] = 0
        cleanup['leases'] = ['not-released']
        checks = disk_quota.checks(result, at_limit, cleanup)
        self.assertFalse(checks['filled_real_quota'])
        self.assertFalse(checks['cleanup_ready'])

    def test_rejects_an_uncharged_output_inode_or_home(self):
        for name in ('home', 'stdout', 'stderr', 'exit', 'cache-stats', 'var_tmp'):
            result, at_limit, cleanup = self.evidence()
            report = json.loads(result['stdout'])
            report['projects'][name] = 0
            result['stdout'] = json.dumps(report)
            self.assertFalse(disk_quota.checks(result, at_limit, cleanup)['all_writable_paths_charged'])

    def test_stdout_lane_requires_real_allocated_blocks_without_stream_growth(self):
        result, at_limit, cleanup = self.evidence()
        report = json.loads(result['stdout'])
        report.update(lane='output_stdout', output_allocated_bytes=2**30, output_size_bytes=0)
        result['stdout'] = json.dumps(report)
        self.assertTrue(all(disk_quota.checks(result, at_limit, cleanup).values()))
        for change in ({'output_allocated_bytes': 0}, {'output_size_bytes': 2**30}):
            result['stdout'] = json.dumps(dict(report, **change))
            self.assertFalse(disk_quota.checks(result, at_limit, cleanup)['output_inode_was_charged'])

    def test_every_required_lane_builds_a_complete_job_program(self):
        self.assertEqual(disk_quota.LANES, ('tmp', 'home', 'var_tmp', 'output_stdout', 'workspace', 'target', 'reused_target', 'reflink_target'))
        for lane in disk_quota.LANES + (disk_quota.RETAINED_LANE,):
            compile(disk_quota.job(lane), '<quota-job>', 'exec')
        with self.assertRaises(ValueError):
            disk_quota.job('outside-quota')
        compile(disk_quota.POSITIVE_CONTROL, '<root-attribute-control>', 'exec')
        compile(disk_quota.observer('probe-token', True), '<quota-observer>', 'exec')
        compile(disk_quota.DEEP_JOB, '<deep-job>', 'exec')
        compile(disk_quota.BASELINE_JOB, '<baseline-job>', 'exec')
        compile(disk_quota.reflink_seed(64 * 1024**2), '<reflink-seed>', 'exec')
        import quota_adversarial
        compile(quota_adversarial.abort_source('probe-token'), '<abort-probe>', 'exec')
        compile(quota_adversarial.retained_guard({}), '<retained-guard>', 'exec')

    def test_warning_loss_is_not_a_proxy_for_enforcement(self):
        result, at_limit, cleanup = self.evidence()
        at_limit['kernel_hard_limit_warning'] = False
        self.assertTrue(disk_quota.checks(result, at_limit, cleanup)['filled_real_quota'])
        at_limit['kernel_usage']['used_bytes'] = 0
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup)['filled_real_quota'])

    def test_retained_snapshot_needs_real_charged_data_and_both_reserves(self):
        _, observation, _ = self.evidence()
        observation['retained_allocated_bytes'] = 2**30
        self.assertTrue(disk_quota.retained_checks(observation))
        observation['retained_allocated_bytes'] = 0
        self.assertFalse(disk_quota.retained_checks(observation))
        observation['retained_allocated_bytes'] = 2**30
        observation['root_free_bytes'] = disk_quota.RESERVE_BYTES - 1
        self.assertFalse(disk_quota.retained_checks(observation))

    def test_reflink_bound_uses_independent_physical_allowance(self):
        mib = 1024**2
        observation = dict(released_logical_bytes=256*mib, bytes_written=960*mib, retained_allocated_bytes=960*mib,
                           lease=dict(initial_blocks=320*mib//512, blocks=1024*mib//512),
                           before=dict(root_free_bytes=64*1024*mib, xfs_free_bytes=64*1024*mib, physical_allowance_bytes=1024*mib),
                           root_free_bytes=64*1024*mib-960*mib, xfs_free_bytes=64*1024*mib-960*mib)
        self.assertTrue(disk_quota.reflink_bound(observation))
        observation['retained_allocated_bytes'] = 2*1024*mib
        self.assertFalse(disk_quota.reflink_bound(observation))

    def charge_evidence(self, lane='home'):
        """A lane that wrote CHARGE_BYTES below its limit, with the kernel charge before and after."""
        result, at_limit, cleanup = self.evidence()
        report = json.loads(result['stdout'])
        report.update(lane=lane, errno=None, bytes_written=disk_quota.CHARGE_BYTES, fill_target_bytes=disk_quota.CHARGE_BYTES)
        result.update(code=0, stdout=json.dumps(report))
        at_limit.update(errno=None, bytes_written=disk_quota.CHARGE_BYTES, kernel_hard_limit_warning=False,
                        kernel_usage=dict(hard_bytes=2**30, used_bytes=(100 << 20) + disk_quota.CHARGE_BYTES))
        at_limit['before'] = dict(at_limit['before'], kernel_usage=dict(hard_bytes=2**30, used_bytes=100 << 20))
        return result, at_limit, cleanup

    def test_only_reflink_and_retained_lanes_fill_to_the_real_cap(self):
        self.assertEqual(disk_quota.FULL_LANES, ('reflink_target', disk_quota.RETAINED_LANE))
        for lane in disk_quota.LANES + (disk_quota.RETAINED_LANE,):
            with self.subTest(lane=lane):
                expected = 0 if lane in disk_quota.FULL_LANES else disk_quota.CHARGE_BYTES
                self.assertEqual(disk_quota.fill_bytes(lane), expected)
                self.assertIn('\nQUOTA_FILL_BYTES=' + str(expected) + '\n', disk_quota.job(lane, 'token'))
        # Whole 8 MiB writes and 1 MiB fallocate steps land exactly on the target.
        self.assertEqual(disk_quota.CHARGE_BYTES % (8 << 20), 0)

    def test_charge_lane_passes_when_the_kernel_charged_its_bytes_to_the_job(self):
        checks = disk_quota.checks(*self.charge_evidence(), disk_quota.CHARGE_BYTES)
        self.assertTrue(all(checks.values()), checks)
        # A charge-only lane never claims the at-limit proofs the full lanes make.
        for name in ('job_write_failed', 'filled_real_quota', 'reserve_at_limit', 'reflink_physical_bound'):
            self.assertNotIn(name, checks)
        self.assertNotIn('charge_fill_completed', disk_quota.checks(*self.evidence()))

    def test_charge_lane_rejects_bytes_the_kernel_did_not_charge_to_the_job(self):
        for change in ({'used_bytes': 100 << 20}, {'used_bytes': (100 << 20) + disk_quota.CHARGE_BYTES - (17 << 20)}):
            with self.subTest(change=change):
                result, at_limit, cleanup = self.charge_evidence()
                at_limit['kernel_usage'].update(change)
                self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['fill_charged_to_job_project'])
        result, at_limit, cleanup = self.charge_evidence()
        del at_limit['before']['kernel_usage']
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['fill_charged_to_job_project'])
        result, at_limit, cleanup = self.charge_evidence()
        at_limit['kernel_usage']['hard_bytes'] = 2**31
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['fill_charged_to_job_project'])

    def test_charge_lane_rejects_a_short_failed_or_mislabelled_fill(self):
        for change in ({'errno': errno.EDQUOT}, {'bytes_written': disk_quota.CHARGE_BYTES - (8 << 20)},
                       {'fill_target_bytes': 0}, {'observer_timeout': True}):
            with self.subTest(change=change):
                result, at_limit, cleanup = self.charge_evidence()
                result['stdout'] = json.dumps(dict(json.loads(result['stdout']), **change))
                self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['charge_fill_completed'])
        result, at_limit, cleanup = self.charge_evidence()
        result['code'] = 74
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['charge_fill_completed'])

    def test_charge_lane_still_requires_every_path_in_the_job_project_and_a_charged_stdout(self):
        result, at_limit, cleanup = self.charge_evidence()
        report = json.loads(result['stdout'])
        report['projects']['var_tmp'] = 0
        result['stdout'] = json.dumps(report)
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['all_writable_paths_charged'])
        result, at_limit, cleanup = self.charge_evidence('output_stdout')
        report = dict(json.loads(result['stdout']), output_allocated_bytes=disk_quota.CHARGE_BYTES, output_size_bytes=0)
        result['stdout'] = json.dumps(report)
        self.assertTrue(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['output_inode_was_charged'])
        result['stdout'] = json.dumps(dict(report, output_allocated_bytes=disk_quota.CHARGE_BYTES - 4096))
        self.assertFalse(disk_quota.checks(result, at_limit, cleanup, disk_quota.CHARGE_BYTES)['output_inode_was_charged'])

    def control(self, supported=True):
        return dict(request=0x401c5820, clear_succeeded=True, before_flags=0x200, after_flags=0,
                    before_project=0, after_project=0, file_setattr_supported=supported,
                    file_setattr_errno=0 if supported else errno.ENOSYS, kernel_release='6.17.0')

    def test_positive_control_requires_a_real_inheritance_transition(self):
        control = self.control()
        self.assertTrue(disk_quota.positive_control_passed(control))
        for change in ({'clear_succeeded': False}, {'before_flags': 0}, {'after_flags': 0x200}, {'after_project': 1}):
            self.assertFalse(disk_quota.positive_control_passed(dict(control, **change)))

    def test_file_setattr_requires_supported_unfiltered_control_for_pass(self):
        result, _, _ = self.evidence()
        self.assertEqual(disk_quota.file_setattr_status(result, self.control()), 'PASS')
        self.assertEqual(disk_quota.file_setattr_status(result, self.control(False)), 'INCONCLUSIVE')
        self.assertEqual(disk_quota.file_setattr_status(result, {}), 'FAIL')

    def test_file_setattr_wrong_refusal_or_missing_job_is_not_inconclusive(self):
        result, _, _ = self.evidence()
        report = json.loads(result['stdout'])
        report['inheritance']['file_setattr_errno'] = errno.EPERM
        result['stdout'] = json.dumps(report)
        self.assertEqual(disk_quota.file_setattr_status(result, self.control(False)), 'FAIL')
        self.assertEqual(disk_quota.file_setattr_status({}, self.control(False)), 'FAIL')

    def partial_fill_job(self):
        """The tmp lane's job result from the live b5501621 run: a 256 MiB partial fill that exited 0."""
        return dict(code=0, stdout=json.dumps({
            'phase': 'at-limit', 'lane': 'tmp', 'errno': None, 'bytes_written': 268435456, 'fill_target_bytes': 268435456,
            'inheritance': {'project': 2, 'child_project': 2, 'setters_refused': [1075599392, 1074292226, 1074030082],
                            'file_setattr_errno': 38},
            'projects': {name: 2 for name in ('workspace', 'target', 'home', 'tmp', 'var_tmp', 'stdout', 'stderr', 'exit', 'cache-stats')},
            'baseline_bytes': 0, 'baseline_project_ids': [], 'released_logical_bytes': 0}))

    def test_file_setattr_is_judged_without_the_fill_outcome(self):
        import probe
        result = self.partial_fill_job()
        live_control = dict(self.control(False), kernel_release='6.8.0-138-generic')
        status = disk_quota.file_setattr_status(result, live_control)
        self.assertEqual(status, 'INCONCLUSIVE')
        row = {'name': 'disk_quota.tmp.file_setattr_refusal', 'status': status, 'evidence': {'control': live_control}}
        self.assertEqual(probe.tolerated_inconclusive(row), 'file_setattr absent on kernel 6.8.0-138-generic (ENOSYS); not applicable')
        self.assertEqual(disk_quota.file_setattr_status(result, self.control()), 'PASS')
        # A full lane's EDQUOT exit and a failed fill leave the verdict unchanged.
        for code, fill_errno in ((73, errno.EDQUOT), (74, errno.EIO)):
            with self.subTest(code=code):
                changed = dict(result, code=code, stdout=json.dumps(dict(json.loads(result['stdout']), errno=fill_errno)))
                self.assertEqual(disk_quota.file_setattr_status(changed, live_control), 'INCONCLUSIVE')
                self.assertEqual(disk_quota.file_setattr_status(changed, self.control()), 'PASS')

    def test_file_setattr_still_requires_the_job_refusals(self):
        for change in ({'file_setattr_errno': errno.EPERM}, {'file_setattr_errno': None}, {'file_setattr_errno': True},
                       {'file_setattr_errno': '38'}, {'setters_refused': [0x401c5820, 0x40086602]}, {'setters_refused': None}):
            with self.subTest(change=change):
                result = self.partial_fill_job()
                report = json.loads(result['stdout'])
                report['inheritance'].update(change)
                result['stdout'] = json.dumps(report)
                self.assertEqual(disk_quota.file_setattr_status(result, self.control()), 'FAIL')
                self.assertEqual(disk_quota.file_setattr_status(result, self.control(False)), 'FAIL')
        for stdout in (json.dumps({'errno': None}), json.dumps([]), json.dumps({'inheritance': []}), ''):
            with self.subTest(stdout=stdout):
                self.assertEqual(disk_quota.file_setattr_status(dict(code=0, stdout=stdout), self.control()), 'FAIL')


class RecordedC7c6127fTests(unittest.TestCase):
    """The phased prepared-image verify at c7c6127f (fixtures/boat-verify-c7c6127f.json)."""

    @staticmethod
    def recorded():
        return json.loads(pathlib.Path(__file__).with_name('fixtures').joinpath('boat-verify-c7c6127f.json').read_text())

    def test_recorded_large_stop_is_named_never_attempted_with_boat_snapshot_progress(self):
        case = self.recorded()['large_quota_stop']
        progress = disk_quota.stop_progress(case['samples'], case['stop_operation'])
        self.assertEqual(progress['classification'], 'boat_stop_never_attempted')
        self.assertEqual((progress['samples'], progress['stop_id'], progress['stop_attempts']),
                         (44, 'stop_81053a3051114291', []))
        self.assertEqual(progress['requested_at'], '2026-10-25T04:19:52.690Z')
        # One periodic snapshot finished during the wait; the next one began a
        # second later and was still running when the phase budget ran out.
        self.assertEqual(progress['snapshots_completed_during_wait'], ['2026-10-25T04:29:26.827Z'])
        self.assertEqual(progress['snapshot_attempts_after_request'], ['2026-10-25T04:29:27.847Z'])
        self.assertEqual(progress['last_snapshot_status'], 'in_progress')
        reason = disk_quota.stop_progress_reason(progress, case['wait'])
        for text in ('boat_stop_never_attempted', 'stop_81053a3051114291', '2026-10-25T04:19:52.690Z', 'all 44 samples',
                     '2026-10-25T04:29:26.827Z', '2026-10-25T04:29:27.847Z', 'phase budget ended the wait after 1284 s'):
            self.assertIn(text, reason)

    def test_recorded_default_stop_that_completed_is_not_called_never_attempted(self):
        case = self.recorded()['default_quota_stop']
        progress = disk_quota.stop_progress(case['samples'], case['stop_operation'])
        self.assertEqual(progress['classification'], 'boat_stop_attempted_not_completed')
        self.assertEqual(progress['stop_attempts'], ['2026-10-25T04:25:38.920Z'])
        # The archive followed the periodic snapshots Boat finished meanwhile.
        self.assertEqual(progress['snapshots_completed_during_wait'],
                         ['2026-10-25T04:17:10.458Z', '2026-10-25T04:21:39.502Z', '2026-10-25T04:25:38.701Z'])
        self.assertEqual(disk_quota.stop_progress([], None)['classification'], 'unobserved')
        unhealthy = [dict(sample, sandbox=dict(sample['sandbox'], health='degraded'))
                     for sample in self.recorded()['large_quota_stop']['samples']]
        self.assertEqual(disk_quota.stop_progress(unhealthy)['classification'], 'boat_stop_pending_unclassified')

    def test_recorded_two_mib_backing_shrink_is_not_a_reclaimed_target(self):
        case = self.recorded()['default_resume']
        before, after = case['backing_before'], case['backing_after']
        retained = case['retained_at_limit']['retained_allocated_bytes']
        # The resumed sandbox still held the 12.9 GB target; its backing file
        # gave back only 4312 sectors.
        self.assertEqual((before['backing_allocated_blocks'] - after['backing_allocated_blocks']) * 512, 2207744)
        self.assertFalse(disk_quota.backing_allocation_dropped(before, after, retained))
        reclaimed = dict(after, backing_allocated_blocks=before['backing_allocated_blocks'] - retained // 512)
        self.assertTrue(disk_quota.backing_allocation_dropped(before, reclaimed, retained))
        for bad in (None, 0, disk_quota.RECLAIM_SLACK_BYTES):
            self.assertFalse(disk_quota.backing_allocation_dropped(before, reclaimed, bad))

    def run_release(self, root, observation, *, ends_after_polls=1, wait_seconds=30):
        """Run the release source against a fixture runner state; the job's
        record gains a terminal after `ends_after_polls` waits (None: never)."""
        record = root / 'jobs' / observation['job_id'] / 'record.json'
        polls = []
        def pause(_seconds):
            polls.append(1)
            if ends_after_polls is not None and len(polls) >= ends_after_polls:
                record.write_text(json.dumps({'terminal': {'outcome': {'type': 'exit', 'code': 73}},
                                              'process': {'unit': '00000000-0000-4000-8000-000000000002'}}))
        source = quota_adversarial.retained_release(observation, wait_seconds=wait_seconds, state=str(root))
        output = io.StringIO()
        with patch('time.sleep', side_effect=pause), patch('subprocess.run', return_value=Mock(returncode=3)), \
             contextlib.redirect_stdout(output):
            exec(compile(source, '<retained-release>', 'exec'), {})
        return json.loads(output.getvalue())

    def test_recorded_retained_job_is_released_after_resume_and_ends(self):
        observation = self.recorded()['default_resume']['retained_at_limit']
        for token, ends, released, passed in ((observation['probe_token'], 1, True, True),
                                              ('another-probe', 1, False, False),
                                              (observation['probe_token'], None, True, False)):
            with self.subTest(token=token, ends=ends), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                marker = root / 'sandboxes' / observation['job_id'] / 'home/tmp/boat-quota-at-limit.json'
                marker.parent.mkdir(parents=True)
                published = json.dumps({'phase': 'at-limit', 'lane': 'retained_target', 'probe_token': token}).encode()
                marker.write_bytes(published.ljust(4096, b' '))
                record = root / 'jobs' / observation['job_id'] / 'record.json'
                record.parent.mkdir(parents=True)
                # Recorded at 04:27:48: the job had started and had no terminal.
                record.write_text(json.dumps({'started_ms': 1, 'terminal': None,
                                              'process': {'unit': '00000000-0000-4000-8000-000000000002'}}))
                result = self.run_release(root, observation, ends_after_polls=ends, wait_seconds=30 if ends else 0)
                self.assertEqual(result['released'], released)
                self.assertEqual(marker.read_bytes()[:1] == b'!', released)
                self.assertFalse(result['terminal_before'])
                self.assertEqual(disk_quota.retained_release_passed(result), passed)


if __name__ == '__main__':
    unittest.main()
