#!/usr/bin/env python3
"""Portable source checks for the Windows trusted-fetch broker boundaries."""
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
WINDOWS = ROOT / "crates/motor-runner/src/windows"


class TrustedPrepLaunchTests(unittest.TestCase):
    def assertCodeIn(self, fragment, source):
        def normalize(text):
            return re.sub(r",(?=[)\]])", "", "".join(text.split()))
        unittest.TestCase.assertIn(self, normalize(fragment), normalize(source))

    def setUp(self):
        self.prep = (WINDOWS / "trusted_prep.rs").read_text()
        self.launch = (WINDOWS / "trusted_prep/launch.rs").read_text()
        self.session = (WINDOWS / "trusted_prep/launch/session.rs").read_text()
        self.process = (WINDOWS / "trusted_prep/launch/process.rs").read_text()

    def test_station_switch_is_only_in_one_shot_broker_before_runtime(self):
        main = (ROOT / "crates/motor-runner/src/main.rs").read_text()
        self.assertLess(main.index('arg == "prep-launch"'), main.index("tokio::runtime::Builder"))
        self.assertNotIn("SetProcessWindowStation", self.prep)
        self.assertCodeIn('broker.arg("prep-launch")', self.launch)
        self.assertCodeIn("SetProcessWindowStation-private", self.session)
        self.assertCodeIn("SetProcessWindowStation-restore", self.session)

    def test_broker_requires_system_before_reading_protected_plan(self):
        entry = self.launch.split("pub(super) fn entry(", 1)[1]
        self.assertLess(entry.index("session::require_system()?"), entry.index("session::read_private_plan"))
        self.assertCodeIn('sid != "S-1-5-18"', self.session)
        self.assertCodeIn('descriptor("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)")', self.session)
        self.assertCodeIn('root.join("state/prep-launch")', self.launch)
        self.assertCodeIn("session::write_private(&path", self.launch)
        self.assertCodeIn("session::private_directory(root, &directory)", self.launch)
        self.assertCodeIn("private_handle(directory, true)", self.session)
        self.assertCodeIn("private_handle(path, false)", self.session)

    def test_private_desktop_and_restriction_use_the_logon_not_user_sid(self):
        self.assertCodeIn("CreateWindowStationW", self.session)
        self.assertCodeIn("CreateDesktopExW", self.session)
        self.assertCodeIn("D:P(A;;GA;;;{logon_sid})", self.session)
        self.assertCodeIn("ConvertStringSidToSidW(wide(&logon_sid)", self.session)
        self.assertNotIn("ConvertStringSidToSidW(wide(&user_sid)", self.session)
        self.assertCodeIn("DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED", self.session)
        self.assertCodeIn("CloseDesktop", self.session)
        self.assertCodeIn("CloseWindowStation", self.session)
        self.assertCodeIn("window station must be non-interactive", self.session)
        self.assertCodeIn("LABEL_SECURITY_INFORMATION", self.session)
        self.assertCodeIn('descriptor("S:(ML;;NW;;;ME)")', self.session)
        self.assertCodeIn("logon == user_sid", self.session)

    def test_profile_and_noninherited_user_environment_have_cleanup(self):
        self.assertCodeIn("LoadUserProfileW(token.0, &mut info)", self.session)
        self.assertCodeIn("UnloadUserProfile(self.token.0, self.handle)", self.session)
        self.assertCodeIn("CreateEnvironmentBlock(&mut pointer, self.token.0, 0)", self.session)
        self.assertCodeIn("DestroyEnvironmentBlock(self.0)", self.session)
        self.assertCodeIn("motor_winagent::environment::parse_block", self.session)
        self.assertCodeIn("env.retain", self.session)
        self.assertCodeIn("for (key, value) in &plan.env", self.session)
        self.assertCodeIn('("TEMP", temp.clone())', self.session)
        self.assertCodeIn('("TMP", temp.clone())', self.session)
        self.assertCodeIn("StagingGrant::new(&temp, sid.0)", self.session)

    def test_fetch_cache_writes_stay_in_staging_and_system_publishes(self):
        self.assertCodeIn('staging.join("fetch-temp/cargo-home")', self.prep)
        self.assertCodeIn('.env("CARGO_HOME", &fetch_cache)', self.prep)
        self.assertNotIn('.env("CARGO_HOME", &cache)', self.prep)
        self.assertCodeIn("copy_tree(&source, &cache.join(name))", self.prep)
        self.assertCodeIn("launch::check_path(&vm.layout_root, &source)", self.prep)
        control = (HERE / "hyperv.ps1").read_text()
        self.assertCodeIn('Grant "$Root\\state\\ally\\prep-cargo" \'(OI)(CI)RX\' $sid', control)
        self.assertNotIn('Grant "$Root\\state\\ally\\prep-cargo" \'(OI)(CI)M\' $sid', control)

    def test_explicit_stdio_handle_list_and_suspended_job_assignment(self):
        self.assertCodeIn("PROC_THREAD_ATTRIBUTE_HANDLE_LIST", self.process)
        self.assertCodeIn("startup.lpAttributeList = attributes.pointer", self.process)
        self.assertCodeIn("startup.StartupInfo.lpDesktop = desktop.as_mut_ptr()", self.process)
        self.assertCodeIn("CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT", self.process)
        self.assertLess(self.process.index("AssignProcessToJobObject"), self.process.index("ResumeThread"))
        self.assertCodeIn("ActiveProcessLimit = 32", self.process)
        self.assertCodeIn("JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE", self.process)
        self.assertCodeIn("accounting.ActiveProcesses == 0", self.process)
        self.assertLess(self.launch.index("let child = process::run"), self.launch.index("context.finish()"))
        self.assertCodeIn("if !released", self.launch)
        self.assertCodeIn("std::mem::forget(context)", self.launch)
        self.assertCodeIn("*self.released = true", self.process)

    def test_native_probe_denies_profile_and_runs_actual_cargo_version(self):
        self.assertCodeIn('"restricted fetch token must not write the loaded profile"', self.prep)
        self.assertCodeIn('"prep TEMP/TMP must point inside staging"', self.prep)
        self.assertCodeIn("let context = launch::probe_context()?", self.prep)
        self.assertCodeIn('version.arg("--version")', self.prep)
        self.assertCodeIn("launch::run(config, &version, &stage, true)", self.prep)
        self.assertCodeIn('stdout.starts_with("cargo ")', self.prep)
        self.assertCodeIn("native-trusted-fetch-cargo-version", self.prep)

    def test_child_exit_diagnostics_use_unsigned_hex_and_ntstatus_name(self):
        status = (WINDOWS / "process_status.rs").read_text()
        self.assertCodeIn("STATUS_DLL_INIT_FAILED", status)
        self.assertCodeIn('0x{code:08X}', status)
        self.assertCodeIn("process_status::describe(code)", self.launch)
        self.assertCodeIn('"lp_desktop"', self.session)
        self.assertCodeIn('"profile_loaded"', self.launch)
        self.assertCodeIn('"exit_status"', self.launch)

    def test_matrix_variants_are_opt_in_and_limited_to_fixed_commands(self):
        self.assertCodeIn("DiagnosticPolicy::None, false", self.launch)
        self.assertCodeIn('plan.args != ["--version"]', self.launch)
        self.assertCodeIn('plan.root.join("prep-toolchain/bin/cargo.exe")', self.launch)
        self.assertCodeIn("plan.diagnostic_policy == DiagnosticPolicy::Everyone", self.session)
        self.assertCodeIn('wide("S-1-1-0")', self.session)
        self.assertCodeIn("plan.diagnostic_policy == DiagnosticPolicy::ExplicitDesktopWrites", self.session)
        self.assertCodeIn("<no child stderr>", self.launch)

    def test_matrix_reads_actual_token_and_ui_permissions_for_seven_cells(self):
        details = self.session.split("fn token_details(", 1)[1].split("fn object_dacl(", 1)[0]
        for symbol in ("TokenRestrictedSids", "IsTokenRestricted(token)", "TokenIntegrityLevel", "TokenSessionId"):
            self.assertCodeIn(symbol, details)
        self.assertCodeIn("token_details(actual_token", self.session)
        for symbol in ("object_dacl(desktop.station)", "object_dacl(desktop.desktop)"):
            self.assertCodeIn(symbol, self.session)
        dacl = self.session.split("fn object_dacl(", 1)[1]
        self.assertCodeIn("GetSecurityInfo", dacl)
        self.assertCodeIn("GetAce", dacl)
        self.assertCodeIn("ace.Mask", dacl)
        for name in ("probe-base", "probe-write-restricted", "cargo-version-base", "cargo-version-write-restricted", "rustc-version-write-restricted", "cargo-version-logon-and-everyone-diagnostic-only", "cargo-version-explicit-desktop-write-mask-diagnostic-only"):
            self.assertCodeIn(name, self.prep)
        self.assertCodeIn("launch::diagnostic(config, &command", self.prep)
        self.assertCodeIn("production_policy_unchanged", self.prep)

    def test_matrix_system_wrapper_returns_before_full_qualification(self):
        wrapper = (HERE / "run-system.ps1").read_text()
        qualify = (HERE / "qualify.ps1").read_text()
        self.assertCodeIn("[switch]$PrepLaunchMatrix", wrapper)
        self.assertCodeIn("if ($PrepLaunchMatrix)", qualify)
        matrix = qualify.split("if ($PrepLaunchMatrix)", 1)[1].split('$parent = "$Root', 1)[0]
        self.assertCodeIn("Runner-Check 'qualify-prep-matrix'", matrix)
        self.assertCodeIn("Copy-QualificationEvidence", matrix)
        self.assertCodeIn("return", matrix)
        self.assertNotIn("Get-FileHash", matrix)

    def test_default_dacl_change_is_diagnostic_only_with_actual_readbacks(self):
        self.assertCodeIn("if plan.diagnostic_policy.changes_default_dacl()", self.session)
        self.assertCodeIn("set_diagnostic_default_dacl(actual_token, &user_sid, &logon_sid)", self.session)
        self.assertCodeIn('D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)(A;;GA;;;{logon_sid})', self.session)
        self.assertCodeIn("SetTokenInformation(token, TokenDefaultDacl", self.session)
        self.assertCodeIn('"token_default_dacl_before":default_dacl_before', self.session)
        self.assertCodeIn('"default_dacl":token_default_dacl(token)?', self.session)
        self.assertCodeIn("acl_details(dacl)", self.session)
        self.assertCodeIn('wide("S-1-5-12")', self.session)
        self.assertCodeIn("plan.diagnostic_policy.adds_restricted_code()", self.session)

    def test_fetch_matrix_cell_is_fixed_offline_fixture_not_arbitrary_fetch(self):
        for name in ("cargo-version-logon-default-dacl-diagnostic-only", "cargo-version-logon-and-restricted-code-diagnostic-only", "cargo-version-logon-restricted-code-default-dacl-diagnostic-only", "cargo-fetch-logon-restricted-code-default-dacl-diagnostic-only"):
            self.assertCodeIn(name, self.prep)
        self.assertCodeIn("plan.diagnostic_policy == DiagnosticPolicy::FetchDefaultDaclRestrictedCode", self.launch)
        self.assertCodeIn('["fetch", "--locked", "--offline", "--manifest-path",', self.launch)
        self.assertCodeIn('plan.cwd.join("fetch-manifests/matrix-fixture/Cargo.toml")', self.launch)
        self.assertCodeIn('command.args(["fetch", "--locked", "--offline", "--manifest-path"])', self.prep)
        self.assertCodeIn('command.env("RUSTC", &rustc)', self.prep)

    def test_write_probe_uses_the_same_session_and_records_allowed_writes_too(self):
        self.assertCodeIn("diagnostic_write_probe(&plan, &context", self.launch)
        self.assertCodeIn("process::run(&probe, context, released)", self.launch)
        self.assertCodeIn('"diagnostic-writes"', self.launch)
        self.assertCodeIn('"same_token_as_primary":true', self.launch)
        self.assertCodeIn("Sha256::digest(fs::read(std::env::current_exe()?)?)", self.launch)
        self.assertCodeIn('"diagnostic-writes" =>', self.prep)
        self.assertCodeIn('Ok(()) => { fs::remove_file(path)?; Ok("allowed") }', self.prep)
        self.assertCodeIn('"outside_write":attempt(outside)?', self.prep)
        self.assertCodeIn('"profile_write":attempt(&profile)?', self.prep)
        self.assertCodeIn('"write_surfaces":surfaces', self.prep)
        self.assertCodeIn("let outcome = attempt(&path)?", self.prep)
        self.assertNotIn("directory.exists()", self.prep)
        self.assertCodeIn('"staging_write_probe_passed":boundary', self.prep)


if __name__ == "__main__":
    unittest.main()
