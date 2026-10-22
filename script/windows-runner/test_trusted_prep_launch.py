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
        # Production desktops stay Medium; only a lowbox diagnostic (package SID
        # present) labels them Low for its Low-integrity child.
        self.assertCodeIn('descriptor(if package.is_some() { "S:(ML;;NW;;;LW)" } else { "S:(ML;;NW;;;ME)" })', self.session)
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
        # The grants loop over the account SID and, for lowbox diagnostics only,
        # the package SID; the persistent cache stays Read for both.
        self.assertCodeIn('$trustees = @($sid) + @($PackageSid | Where-Object { $_ })', control)
        self.assertCodeIn('Grant "$Root\\state\\ally\\prep-cargo" Read $trustee', control)
        self.assertNotIn('prep-cargo" Modify', control)

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
        matrix = qualify.split("if ($PrepLaunchMatrix)", 1)[1].split("if ($OfflineCargoOnly)", 1)[0]
        self.assertCodeIn("Runner-Check 'qualify-prep-matrix'", matrix)
        self.assertCodeIn("Copy-QualificationEvidence", matrix)
        self.assertCodeIn("return", matrix)
        self.assertNotIn("Get-FileHash", matrix)

    def test_offline_cargo_diagnostic_is_separate_and_keeps_lowbox_out(self):
        wrapper = (HERE / "run-system.ps1").read_text()
        qualify = (HERE / "qualify.ps1").read_text()
        main = (ROOT / "crates/motor-runner/src/main.rs").read_text()
        library = (ROOT / "crates/motor-runner/src/lib.rs").read_text()
        windows = (ROOT / "crates/motor-runner/src/windows.rs").read_text()
        self.assertCodeIn("[switch]$OfflineCargoOnly", wrapper)
        self.assertCodeIn("$PrepLaunchMatrix -or $AccessChecksOnly -or $LowboxLaunch", wrapper)
        self.assertCodeIn('"qualify-prep-offline-cargo" => motor_runner::hyperv_qualify_prep_offline_cargo(config)', main)
        self.assertCodeIn("pub fn hyperv_qualify_prep_offline_cargo", library)
        self.assertCodeIn("trusted_prep::offline_cargo_entry(config)", windows)
        offline = qualify.split("if ($OfflineCargoOnly)", 1)[1].split('$parent = "$Root', 1)[0]
        self.assertCodeIn("Runner-Check 'qualify-prep-offline-cargo'", offline)
        self.assertCodeIn("Copy-QualificationEvidence", offline)
        self.assertNotIn("LowboxLaunch", offline)

    def test_offline_cargo_probe_reports_the_sparse_index_and_lockfile_archive(self):
        self.assertCodeIn('"registry/index/<registry-id>/.cache/3/c/cfg-if"', self.prep)
        self.assertCodeIn('"registry/cache/<registry-id>/cfg-if-1.0.4.crate"', self.prep)
        self.assertCodeIn('"checksum_matches_lockfile": matches', self.prep)
        self.assertCodeIn('"CARGO_NET_OFFLINE": "true"', self.prep)
        self.assertCodeIn('"fetch", "--locked", "--offline", "--manifest-path"', self.prep)
        self.assertCodeIn('"fetch", "--locked", "--manifest-path"', self.prep)
        self.assertCodeIn('launch::DiagnosticPolicy::OfflineCargo', self.prep)
        self.assertCodeIn("cargo did not start", self.prep)
        self.assertCodeIn("output_mentions_ksecdd", self.prep)
        self.assertCodeIn("exit_code_looks_like_ntstatus", self.prep)
        self.assertCodeIn("&& policy != DiagnosticPolicy::OfflineCargo", self.launch)
        self.assertCodeIn("CARGO_NET_OFFLINE", self.launch)
        self.assertCodeIn("offline-cargo-home", self.launch)

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
        self.assertCodeIn('plan.args[..4] != ["fetch", "--locked", "--offline", "--manifest-path"]', self.launch)
        self.assertCodeIn('Path::new(&plan.args[4]) != plan.cwd.join("fetch-manifests/matrix-fixture").join("Cargo.toml")', self.launch)
        self.assertCodeIn('command.args(["fetch", "--locked", "--offline", "--manifest-path"])', self.prep)
        self.assertCodeIn('command.env("RUSTC", &rustc)', self.prep)

    def test_write_probe_uses_the_same_session_and_records_allowed_writes_too(self):
        self.assertCodeIn("diagnostic_write_probe(&plan, &context", self.launch)
        self.assertCodeIn("process::run(&probe, context, released, &mut probe_created, &mut probe_started)", self.launch)
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

    def test_loader_debugging_is_diagnostic_only_and_reads_native_messages(self):
        process = self.process
        trace = (WINDOWS / "trusted_prep/launch/process/trace.rs").read_text()
        self.assertCodeIn("diagnostic_loader_trace: retain_failure", self.launch)
        self.assertCodeIn("if plan.diagnostic_loader_trace { DEBUG_ONLY_THIS_PROCESS } else { 0 }", process)
        self.assertCodeIn("if plan.diagnostic_loader_trace {", process)
        self.assertCodeIn("trace::wait(plan, tree.process.0, deadline)", process)
        for api in ("WaitForDebugEventEx", "ContinueDebugEvent", "ReadProcessMemory", "OUTPUT_DEBUG_STRING_EVENT", "LOAD_DLL_DEBUG_EVENT", "EXCEPTION_DEBUG_EVENT"):
            self.assertCodeIn(api, trace)
        self.assertCodeIn("super::session::kernel_object_details(handle)", process)
        self.assertCodeIn("inspect(tree.process.0)", process)
        self.assertCodeIn("inspect(thread.0)", process)
        self.assertCodeIn('"CARGO_HOME":env.get("CARGO_HOME")', self.session)
        self.assertCodeIn('"RUSTUP_HOME":env.get("RUSTUP_HOME")', self.session)
        self.assertCodeIn("DefaultDaclProbeFirst", self.launch)
        self.assertCodeIn("DefaultDaclCargoTwice", self.launch)

    def test_loader_snap_flags_are_reverted_and_no_external_debugger_is_needed(self):
        flags = (HERE / "loader-snaps.ps1").read_text()
        self.assertCodeIn("@('cargo.exe','rustc.exe')", flags)
        self.assertCodeIn("$flags -bor 2", flags)
        self.assertLess(flags.index("Save-RunnerLoaderState $snapshot $state"), flags.index("$key.SetValue('GlobalFlag', $entry.applied_value"))
        self.assertCodeIn("Restore-RunnerLoaderState $Registry $state.entries", flags)
        self.assertCodeIn("$key.SetValue('GlobalFlag', $entry.original, $kind)", flags)
        self.assertCodeIn("$key.DeleteValue('GlobalFlag', $false)", flags)
        self.assertCodeIn("$Registry.DeleteSubKey($path, $false)", flags)
        self.assertCodeIn("Invoke-RunnerLoaderSnaps $Evidence", (HERE / "qualify.ps1").read_text())
        fixtures = (HERE / "test_scripts.ps1").read_text()
        self.assertCodeIn("loader-snaps-restores-original-type-and-missing-key", fixtures)
        self.assertCodeIn("loader-snaps-restores-after-failed-native-action", fixtures)
        self.assertCodeIn("'loader-snaps.ps1'", (HERE / "install.ps1").read_text())

    def test_safe_cwd_is_an_exact_same_token_comparison_not_a_new_grant(self):
        self.assertCodeIn("diagnostic_safe_cwd: policy.rng_mode().is_some()", self.launch)
        self.assertCodeIn("plan.cwd.join(\"fetch-temp\")", self.process)
        self.assertCodeIn("let working_directory = if plan.diagnostic_safe_cwd", self.process)
        self.assertCodeIn("safe.diagnostic_safe_cwd = true", self.launch)
        self.assertCodeIn("process::run(&safe, &context", self.launch)
        self.assertCodeIn("diagnostic_rng_call(&plan, &context", self.launch)
        self.assertCodeIn("process::run(&rng, context, released, &mut rng_created, &mut rng_started)", self.launch)
        self.assertCodeIn('"same_token_as_safe_cwd_cargo":true', self.launch)
        self.assertCodeIn("DefaultDaclSafeCwdPair", self.prep)

    def test_direct_rng_probes_are_dynamic_and_keep_return_types_distinct(self):
        rng = (WINDOWS / "trusted_prep/launch/rng.rs").read_text()
        for value in ("ProcessPrng", "BCryptGenRandom", "LoadLibraryExW", "LOAD_LIBRARY_SEARCH_SYSTEM32", "GetProcAddress", "FreeLibrary", "BCRYPT_USE_SYSTEM_PREFERRED_RNG"):
            self.assertCodeIn(value, rng)
        self.assertCodeIn('"return_type"] = json!("BOOL")', rng)
        self.assertCodeIn('"return_type"] = json!("NTSTATUS")', rng)
        self.assertCodeIn('"random_bytes_logged":false', rng)
        self.assertCodeIn("write_volatile(byte, 0)", rng)
        self.assertCodeIn("call(null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, 0x00000002)", rng)
        probe = self.prep.split("pub(crate) fn probe(", 1)[1].split("pub(super) fn matrix_entry", 1)[0]
        self.assertLess(probe.index("return launch::rng_probe"), probe.index("let profile"))
        self.assertCodeIn("DefaultDaclProcessPrng", self.prep)
        self.assertCodeIn("DefaultDaclBCryptRng", self.prep)

    def test_native_access_trace_reads_targets_rights_and_actual_return_status(self):
        native = (WINDOWS / "trusted_prep/launch/process/trace/native_calls.rs").read_text()
        trace = (WINDOWS / "trusted_prep/launch/process/trace.rs").read_text()
        for value in ("NtCreateFile", "NtOpenFile", "NtOpenSection", "NtOpenKey", "NtOpenProcess", "NtOpenThread", "NtDeviceIoControlFile", "NtConnectPort"):
            self.assertCodeIn(value, native)
        for value in ("GetThreadContext", "SetThreadContext", "ReadProcessMemory", "WriteProcessMemory", "VirtualProtectEx", "FlushInstructionCache", "DuplicateHandle", "NtQueryObject"):
            self.assertCodeIn(value, native)
        self.assertCodeIn("native_call_trace::ntstatus(context.0.Rax)", native)
        self.assertCodeIn("native_call_trace::matches_return", native)
        self.assertCodeIn('"desired_access":access_text(call.access)', native)
        self.assertCodeIn("Some(context.Rdx as u32)", native)
        self.assertCodeIn('"pending_completion_not_observed":code==0x103', native)
        self.assertCodeIn("self.target(&api, &context.0)", native)
        self.assertCodeIn("plan.diagnostic_native_calls.then", trace)
        self.assertCodeIn("calls.exception(event.dwThreadId", trace)
        self.assertCodeIn("calls.module_loaded", trace)
        self.assertCodeIn("self.remote_exports(base)", native)
        self.assertCodeIn("mapping\":\"remote PE export table", native)
        self.assertCodeIn("self.pause_others(tid)", native)
        self.assertCodeIn("context.0.Rip = address", native)
        self.assertNotIn("context.0.Rax =", native)

    def test_native_trace_aborts_cleanly_instead_of_misattributing_unwinds(self):
        native = (WINDOWS / "trusted_prep/launch/process/trace/native_calls.rs").read_text()
        trace = (WINDOWS / "trusted_prep/launch/process/trace.rs").read_text()
        self.assertCodeIn("interrupted an instrumented call/step", native)
        exception = native.split("pub(super) fn exception(",1)[1].split("pub(super) fn abort(",1)[0]
        self.assertCodeIn("step.suspended.release()", exception)
        self.assertCodeIn("self.steps.insert(tid, step)", exception)
        self.assertCodeIn("native_call_trace::defer_breakpoint(owner, tid)", exception)
        self.assertCodeIn("native-call-deferred-breakpoint", exception)
        self.assertCodeIn("self.patch(address, self.points[&address].original)?", exception)
        self.assertCodeIn("context.0.EFlags |= 0x100", exception)
        self.assertCodeIn("GetExitCodeThread", native)
        self.assertCodeIn("pub(super) fn abort", native)
        self.assertCodeIn("self.patch(*address, point.original)", native)
        self.assertCodeIn("self.calls.clear()", native)
        self.assertLess(trace.index("calls.abort("), trace.index('check("ContinueDebugEvent"'))
        point = native.split("fn point(", 1)[1].split("fn site(", 1)[0]
        self.assertLess(point.index("self.points.insert"), point.index("self.patch(address, 0xcc)"))
        self.assertCodeIn("let exports = self.remote_exports(base)?", native)
        self.assertCodeIn("let mut candidates", native)


if __name__ == "__main__":
    unittest.main()
