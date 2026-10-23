import errno
import json
import unittest

import disk_quota


class DiskQuotaChecks(unittest.TestCase):
    def evidence(self):
        result = dict(code=73, stdout=json.dumps(dict(errno=errno.EDQUOT, inheritance=dict(project=1, child_project=1,
                      setters_refused=[0x401c5820, 0x40086602, 0x40046602]))))
        at_limit = dict(errno=errno.EDQUOT, bytes_written=2**30, lease=dict(blocks=2**21, project=1, growth_bytes=2**30),
                        kernel_hard_limit_warning=True,
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


if __name__ == '__main__':
    unittest.main()
