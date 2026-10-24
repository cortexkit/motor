import errno
import json
import unittest

import disk_quota


class DiskQuotaChecks(unittest.TestCase):
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
