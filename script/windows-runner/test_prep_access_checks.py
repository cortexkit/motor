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
        self.code('New-RunnerPrepAcl (Get-RunnerInheritedDirectoryAcl $Path) $Sid $Role', self.acl)
        self.code('SetSecurityDescriptorBinaryForm($bytes, [Security.AccessControl.AccessControlSections]::Access)', self.acl)
        self.code('New-RunnerPrepDirectory $fixture $sid Stage', self.fixture)
        self.code('New-RunnerPrepDirectory $manifests $sid Read', self.fixture)
        self.assertNotIn('Set-Acl', '\n'.join(line for line in self.fixture.splitlines() if not line.lstrip().startswith('#')))
        self.code('requested_sddl=$requested', self.fixture)
        self.code('actual_sddl=', self.fixture)
        self.code('finally {', self.fixture)
        self.code('$state.cleanup_method = Remove-RunnerAccessFixture $fixture', self.fixture)
        self.code('$state.removed = -not (Test-Path -LiteralPath $fixture)', self.fixture)

    def test_inherited_rules_come_from_an_ordinarily_created_probe(self):
        # A descriptor passed at creation replaces inheritance, so the base must
        # be what a normally created sibling received, then that probe is removed.
        self.code("New-Item -ItemType Directory -Path $probe -ErrorAction Stop", self.acl)
        self.code('try { return Get-Acl -LiteralPath $probe -ErrorAction Stop }', self.acl)
        self.code('finally { Remove-Item -LiteralPath $probe -Force -ErrorAction Stop }', self.acl)
        self.assertNotIn('RawAcl]::new(2,0)', self.acl)

    def test_cleanup_recovers_and_sweeps_leftovers_without_masking_failures(self):
        self.code("try { Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop; return 'removed' }", self.fixture)
        self.code('Reset-RunnerAccessFixtureAcl $Path 0', self.fixture)
        self.code("& takeown.exe /F $Path /A | Out-Null", self.fixture)
        self.code('ReparsePoint', self.fixture)
        self.code("-Filter '.motor-acl-probe-*'", self.fixture)
        self.code("[Guid]::TryParse((Split-Path -Leaf $prior.fixture), [ref]$guid)", self.fixture)
        self.code('(Split-Path -Parent $prior.fixture) -eq $staging', self.fixture)
        self.code('$state.leftovers = @(Remove-RunnerAccessLeftovers $Root (Split-Path -Parent $Evidence))', self.fixture)
        fixtures = (HERE / 'test_scripts.ps1').read_text()
        for name in ('prep-acl-fixture-leftovers-removed', 'prep-acl-fixture-cleanup-recovers-unlistable-tree', "'prep-acl-fixture-cleanup' {", 'prep-creation-descriptor-matches-icacls-and-production-create-then-grant'):
            self.assertIn(name, fixtures)
        self.code("if ($null -eq $failure) { throw }", fixtures)
        self.code("-Filter 'motor-prep-acl-*'", fixtures)

    def test_creation_only_uses_native_create_and_has_independent_icacls_oracle(self):
        self.code('CreateDirectoryW(path, ref attributes)', self.acl)
        self.code('throw new Win32Exception(Marshal.GetLastWin32Error())', self.acl)
        fixtures = (HERE / 'test_scripts.ps1').read_text()
        self.assertIn('prep-creation-descriptor-matches-icacls-and-production-create-then-grant', fixtures)
        self.assertIn('prep-acl-rule-builds-in-memory-for-every-role', fixtures)
        self.assertIn('prep-creation-only-fixture-never-rewrites-an-existing-acl', fixtures)
        self.code("@('Stage','(X)'),@('Read','(OI)(CI)RX'),@('Modify','(OI)(CI)M')", fixtures)
        self.code('icacls.exe $oracle /grant:r', fixtures)
        self.code('Grant-RunnerPrepPath $production $sid $row[0]', fixtures)

    def test_byte_enum_flags_avoid_powershell_enum_arithmetic_and_checks_name_themselves(self):
        # AceFlags is byte-backed; the Ally's PowerShell 5.1 run threw a bare
        # InvalidCastException in this area, so no -bor/-band or [int] cast on it.
        for source in (self.acl, (HERE / 'test_scripts.ps1').read_text()):
            code = '\n'.join(line for line in source.splitlines() if not line.lstrip().startswith('#') and 'INFO prep-acl-legacy' not in line)
            self.assertNotRegex(code, r'AceFlags\]::\w+\s*-b(or|and)')
            self.assertNotIn('[int]$_.AceFlags', code)
        self.code('[Enum]::ToObject([Security.AccessControl.AceFlags], 3)', self.acl)
        fixtures = (HERE / 'test_scripts.ps1').read_text()
        self.code('Write-Host "RUN $Name"', fixtures)
        self.code('throw "FAIL ${Name}:', fixtures)
        self.code('$Ace.AceFlags.value__.ToString(\'x2\')', fixtures)
        self.assertIn('trap { Write-Host "FAILED at', fixtures)

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

    def test_lowbox_screen_is_access_check_only_and_models_lpac_and_low_label(self):
        lowbox = (SRC / 'trusted_prep/launch/session/access_checks/lowbox.rs').read_text()
        for forbidden in ('CreateProcess', 'SetSecurityInfo(', 'NtSetSecurityObject(', 'SetTokenInformation(', 'CreateAppContainerProfile', 'RegSetValue', 'SetNamedSecurityInfo'):
            self.assertNotIn(forbidden, lowbox)
        self.code('NtCreateLowBoxToken(', lowbox)
        self.code('CreateRestrictedToken(base, DISABLE_MAX_PRIVILEGE, 0, null(), 0, null(), 0, null(), &mut filtered)', lowbox)
        self.code('const INTERNET_CLIENT: &str = "S-1-15-3-1";', lowbox)
        self.code('["registryRead", "lpacCryptoServices"]', lowbox)
        self.code('parsed.without_all_application_packages()', lowbox)
        self.code('Some("(ML;OICI;NW;;;LW)")', lowbox)
        self.code('roundtrip_ok = mismatches.is_empty();', lowbox)
        self.code('&& roundtrip_ok', lowbox)
        self.code('decide(device_pointer, check, DEVICE_ACCESS, &file)', lowbox)
        self.code('report["lowbox"] = lowbox::screen(', self.source)
        model = (SRC / 'sddl_model.rs').read_text()
        self.code('sid != "AC" && sid != ALL_APPLICATION_PACKAGES', model)

if __name__ == '__main__':
    unittest.main()
