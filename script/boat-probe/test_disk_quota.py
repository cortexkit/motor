import errno
import contextlib
import io
import json
import unittest
from unittest.mock import Mock, patch

import disk_quota
import quota_adversarial


class DiskQuotaChecks(unittest.TestCase):
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
        before = dict(expected_backing=True, backing_file='/var/lib/motor-boat.xfs', backing_allocated_blocks=1000)
        after = dict(before, backing_allocated_blocks=100)
        self.assertTrue(disk_quota.backing_allocation_dropped(before, after))
        for change in ({'backing_allocated_blocks': 1000}, {'backing_allocated_blocks': 1001},
                       {'backing_allocated_blocks': None}, {'backing_allocated_blocks': True},
                       {'backing_allocated_blocks': -1}, {'expected_backing': False},
                       {'backing_file': '/other.xfs'}):
            with self.subTest(change=change):
                self.assertFalse(disk_quota.backing_allocation_dropped(before, dict(after, **change)))
        self.assertFalse(disk_quota.backing_allocation_dropped({}, after))
        self.assertFalse(disk_quota.backing_allocation_dropped(before, {}))

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


if __name__ == '__main__':
    unittest.main()
