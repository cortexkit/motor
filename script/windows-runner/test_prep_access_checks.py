#!/usr/bin/env python3
"""Source checks for read-only device/individual-right descriptor screening."""
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SRC = ROOT / 'crates/motor-runner/src/windows'


def flat(text):
    return re.sub(r',(?=[)\]])', '', ''.join(text.split()))


class PrepAccessScreenTests(unittest.TestCase):
    def setUp(self):
        self.source = (SRC / 'trusted_prep/launch/session/access_checks.rs').read_text()
        self.acl = (HERE / 'prep-acl.ps1').read_text()
        self.fixture = (HERE / 'prep-access-check.ps1').read_text()

    def code(self, fragment, source):
        self.assertIn(flat(fragment), flat(source))

    def test_query_is_read_only_and_requests_integrity_not_audit_privileges(self):
        self.code('READ_CONTROL | SYNCHRONIZE', self.source)
        self.code('SD_PARTS | LABEL_SECURITY_INFORMATION', self.source)
        self.code('GetSecurityDescriptorSacl', self.source)
        self.code('MapGenericMask', self.source)
        self.code('"allow_write_data":ace.Header.AceType==0 && mask&2!=0', self.source)
        for forbidden in ('SetSecurityInfo(', 'NtSetSecurityObject(', 'SetTokenInformation(', 'AdjustTokenPrivileges(', 'LoadUserProfile(', 'CreateProcessAsUser('):
            self.assertNotIn(forbidden, self.source)
        self.code('GetUserProfileDirectoryW', self.source)

    def test_candidates_use_actual_enabled_account_groups_and_device_aces(self):
        self.code('enabled_groups(base.0, &logon)', self.source)
        self.code('writer_sids.contains(&sid)', self.source)
        self.code('group.Attributes & 4 != 0', self.source)
        self.code('group.Attributes & 0x70 == 0', self.source)
        self.code('vec![logon.clone(), sid]', self.source)
        self.code('CreateRestrictedToken(base, FETCH_TOKEN_FLAGS', self.source)
        self.code('DuplicateTokenEx(token, TOKEN_QUERY', self.source)
        self.code('token_details(restricted.0, FETCH_TOKEN_FLAGS)', self.source)

    def test_individual_write_rights_and_actual_access_status_feed_the_screen(self):
        for value in ('write_data_or_add_file', 'append_or_add_subdirectory', 'write_extended_attributes', 'write_attributes', 'delete', 'write_dacl', 'write_owner', 'delete_child'):
            self.assertIn(value, self.source)
        self.code('access_screen::decision(ok != 0, status != 0, granted, mask)', self.source)
        self.code('access_screen::passes_screen', self.source)
        self.code('"positive_manifest_read_control":read_control', self.source)
        self.code('"ambient_scope_counterexample":ambient', self.source)
        self.code('"screen_complete"', self.source)
        self.code('"baseline_device_model_matches_prior_native_denial"', self.source)

    def test_required_paths_and_replacement_directories_are_not_skipped(self):
        for value in ('windows_temp', 'profile', 'profile_local_temp', 'persistent_prep_cache', 'service_home', 'fixture_manifests', 'fixture_manifest_file', 'toolchain', 'cargo_executable', 'rustc_executable', 'runner_root_outside_staging', 'baselines_directory', 'configured_parent', 'baseline_parent_directory'):
            self.assertIn(value, self.source)
        self.code('vm.baseline_path()', self.source)
        self.assertIn('runner_root_child_outside_staging', self.source)
        self.code('eq_ignore_ascii_case("staging")', self.source)
        self.code('None=>gaps.push(', self.source)
        self.code('"image_discovery_gaps"', self.source)
        self.code('path.descriptor.is_some()', self.source)
        self.code('path.description["security"]["integrity_label_error"].is_null()', self.source)

    def test_fixture_and_production_share_the_descriptor_builder(self):
        control = (HERE / 'hyperv.ps1').read_text()
        self.code('Grant-RunnerPrepPath $Path $Sid $Role', control)
        self.code('New-RunnerPrepAcl (Get-Acl -LiteralPath $Path -ErrorAction Stop) $Sid $Role', self.acl)
        self.code('New-RunnerPrepAcl $BaseAcl $Sid $Role', self.acl)
        self.code('SetSecurityDescriptorBinaryForm($bytes, [Security.AccessControl.AccessControlSections]::Access)', self.acl)
        self.code('New-RunnerPrepDirectory $fixture $base $sid Stage', self.fixture)
        self.code('New-RunnerPrepDirectory $manifests (Get-Acl -LiteralPath $fixture) $sid Read', self.fixture)
        self.assertNotIn('Set-Acl', '\n'.join(line for line in self.fixture.splitlines() if not line.lstrip().startswith('#')))
        self.code('requested_sddl=$requested', self.fixture)
        self.code('actual_sddl=', self.fixture)
        self.code('finally {', self.fixture)
        self.code('Remove-Item -LiteralPath $fixture -Recurse -Force', self.fixture)
        self.code('$state.removed = -not (Test-Path -LiteralPath $fixture)', self.fixture)

    def test_creation_only_uses_native_create_and_has_independent_icacls_oracle(self):
        self.code('CreateDirectoryW(path, ref attributes)', self.acl)
        self.code('throw new Win32Exception(Marshal.GetLastWin32Error())', self.acl)
        fixtures = (HERE / 'test_scripts.ps1').read_text()
        self.assertIn('prep-shared-descriptor-and-inheritance-match-independent-icacls', fixtures)
        self.assertIn('prep-creation-only-fixture-never-rewrites-an-existing-acl', fixtures)
        self.code("@('Stage','(X)'),@('Read','(OI)(CI)RX'),@('Modify','(OI)(CI)M')", fixtures)
        self.code('icacls.exe $oracle /grant:r', fixtures)
        self.code('Grant-RunnerPrepPath $production $sid $row[0]', fixtures)

    def test_read_only_lane_skips_loader_flags_and_runtime_reconciliation(self):
        control = (HERE / 'qualify.ps1').read_text()
        branch = control.split('if ($AccessChecksOnly) {', 1)[1].split('} else {', 1)[0]
        self.code('Invoke-RunnerPrepAccessMatrix $Root $exe $Evidence', branch)
        self.assertNotIn('Invoke-RunnerLoaderSnaps', branch)
        self.assertNotIn('GrantPrep', branch)
        self.code("@('prep-access-check',$Root,$fixture", self.fixture)
        launch = (SRC / 'trusted_prep/launch.rs').read_text().split('pub(super) fn access_check_entry(', 1)[1]
        self.assertNotIn('Runner::open', launch)
        self.assertNotIn('GrantPrep', launch)
        self.code('session::require_system()', launch)
        install = (HERE / 'install.ps1').read_text()
        for name in ('prep-acl.ps1','prep-access-check.ps1'):
            self.assertIn("'"+name+"'", install)


if __name__ == '__main__':
    unittest.main()
