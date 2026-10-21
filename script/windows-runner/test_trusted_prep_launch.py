#!/usr/bin/env python3
"""Portable source checks for the Windows trusted-fetch broker boundaries."""
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
WINDOWS = ROOT / "crates/motor-runner/src/windows"


class TrustedPrepLaunchTests(unittest.TestCase):
    def assertCodeIn(self, fragment, source):
        unittest.TestCase.assertIn(self, "".join(fragment.split()), "".join(source.split()))

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


if __name__ == "__main__":
    unittest.main()
