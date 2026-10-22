#!/usr/bin/env python3
"""Portable checks for Cargo diagnostics run as Windows SYSTEM and registry preparation."""
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SRC = ROOT / "crates/motor-runner/src"
WINDOWS = SRC / "windows"


class TrustedPrepDiagnosticTests(unittest.TestCase):
    def assertCodeIn(self, fragment, source):
        def normalize(text):
            return re.sub(r",(?=[)\]])", "", "".join(text.split()))
        unittest.TestCase.assertTrue(
            self, normalize(fragment) in normalize(source), f"missing source fragment: {fragment!r}"
        )

    def setUp(self):
        self.prep = (WINDOWS / "trusted_prep.rs").read_text()
        self.registry = (WINDOWS / "trusted_prep/local_registry.rs").read_text()
        self.windows = (SRC / "windows.rs").read_text()
        self.main = (ROOT / "crates/motor-runner/src/main.rs").read_text()
        self.library = (ROOT / "crates/motor-runner/src/lib.rs").read_text()
        self.prepare = (WINDOWS / "prep.rs").read_text()
        self.agent = (ROOT / "crates/motor-winagent/src/lib.rs").read_text()
        self.agent_main = (ROOT / "crates/motor-winagent/src/main.rs").read_text()
        self.hyperv = (HERE / "hyperv.ps1").read_text()
        self.install = (HERE / "install.ps1").read_text()
        self.parent_helper = (HERE / "parent-identity.ps1").read_text()
        self.wrapper = (HERE / "run-system.ps1").read_text()
        self.qualify = (HERE / "qualify.ps1").read_text()

    def test_offline_cache_diagnostic_is_a_system_only_lane(self):
        self.assertCodeIn("[switch]$OfflineCargoOnly", self.wrapper)
        self.assertCodeIn('"qualify-prep-offline-cargo" => motor_runner::hyperv_qualify_prep_offline_cargo(config)', self.main)
        self.assertCodeIn("pub fn hyperv_qualify_prep_offline_cargo", self.library)
        self.assertCodeIn("trusted_prep::offline_cargo_entry(config)", self.windows)
        offline = self.qualify.split("if ($OfflineCargoOnly)", 1)[1].split("if ($LocalRegistryOnly)", 1)[0]
        self.assertCodeIn("Runner-Check 'qualify-prep-offline-cargo'", offline)
        self.assertCodeIn("Copy-QualificationEvidence", offline)
        self.assertNotIn("motor-prep", offline)
        self.assertCodeIn('"actor": "SYSTEM runner; Cargo runs only as SYSTEM in this diagnostic"', self.prep)
        self.assertCodeIn('"restricted_motor_prep_cargo_launch_attempted": false', self.prep)

    def test_offline_diagnostic_reports_cargo_cache_and_runs_three_layouts(self):
        self.assertCodeIn('"sparse_index_cache_entries_named_cfg_if": sparse_entries', self.prep)
        self.assertCodeIn('part.as_os_str() == ".cache"', self.prep)
        self.assertCodeIn('"contents_hex_up_to_64_kib": hex_evidence(&bytes)', self.prep)
        self.assertCodeIn('"prefix_escaped_first_96_bytes": format!', self.prep)
        self.assertCodeIn('"crate_archive_only", false, false', self.prep)
        self.assertCodeIn('"archive_plus_registry_config_no_sparse_cache", true, false', self.prep)
        self.assertCodeIn('"complete_cargo_seed", true, true', self.prep)
        self.assertCodeIn('"fetch", "--locked", "--offline", "--manifest-path"', self.prep)
        self.assertCodeIn('"fetch", "--locked", "--manifest-path"', self.prep)
        self.assertNotIn("PrepLaunchMatrix", self.qualify)
        self.assertNotIn("CreateRestrictedToken", self.prep)

    def test_production_preparation_downloads_without_starting_host_cargo(self):
        self.assertCodeIn("execute_with_registry(&plan_path", self.prepare)
        self.assertCodeIn("prepare_local_registry(config, job_staging, &lockfile, routes)", self.prepare)
        prepare_body = self.registry.split("pub(super) fn prepare_for_job(", 1)[1].split("\npub(super) fn prove(", 1)[0]
        self.assertCodeIn('download_allowlisted("index.crates.io"', prepare_body)
        self.assertCodeIn('download_allowlisted("static.crates.io"', prepare_body)
        self.assertNotIn("run_cargo(", prepare_body)
        self.assertNotIn("Command::new(cargo)", prepare_body)
        execute_body = self.prepare.split("pub(crate) fn execute_with_registry(", 1)[1].split("\n}", 1)[0]
        self.assertLess(
            execute_body.index("validate_repository_cargo_sources"),
            execute_body.index("prepare_registry("),
        )
        self.assertCodeIn('"unsupported Git bundle source URL: {url}"', self.prepare)
        lock_validation = self.prepare.split("fn validate_lock(", 1)[1].split("fn git(", 1)[0]
        self.assertLess(
            lock_validation.index("validate_git_source_url(url)?"),
            lock_validation.index("return Err(invalid(format!("),
        )

    def test_runtime_registry_cache_matches_the_installed_private_directory(self):
        self.assertCodeIn(
            'const LOCAL_REGISTRY_CACHE_DIRECTORY: &str = "local-registry-cache"',
            self.prep,
        )
        self.assertCodeIn(
            "config.state_dir().join(LOCAL_REGISTRY_CACHE_DIRECTORY)",
            self.prep,
        )
        self.assertIn(r"state\ally\local-registry-cache", self.install)
        self.assertIn(r'Protect-RunnerPath "$Root\state\ally\local-registry-cache"', self.install)
        self.assertIn(r'Protect-RunnerPath "$Root\staging"', self.install)
        self.assertIn(r"$Root\state\ally\local-registry-cache", self.hyperv)
        private_checks = self.hyperv.split("foreach ($path in @($Root, \"$Root\\state\",", 1)[1].split(") {", 1)[0]
        self.assertIn(r"$Root\state\ally\local-registry-cache", private_checks)
        self.assertIn(r"$Root\staging", private_checks)
        self.assertIn("Required private runner directory is missing", self.hyperv)
        self.assertIn("Assert-RunnerPrivateDirectory $path", self.hyperv)
        self.assertIn("Reparse runner directory refused", self.parent_helper)
        self.assertIn("Runner directory grants access outside SYSTEM/Administrators", self.parent_helper)
        self.assertIn("Runner directory owner must be SYSTEM or Administrators", self.parent_helper)
        self.assertIn("Null runner directory DACL refused", self.parent_helper)

    def test_unsupported_repository_sources_are_refused_before_registry_fetch(self):
        self.assertCodeIn("validate_repository_cargo_sources(&plan.staging)", self.prepare)
        self.assertCodeIn('for key in ["source", "registry", "registries"]', self.prepare)
        self.assertCodeIn("unsupported repository Cargo {key} configuration", self.prepare)
        self.assertCodeIn("repository_source_override_is_named_and_rejected_before_registry_fetch", self.prepare)

    def test_guest_rewrites_host_registry_path_to_absolute_cargo_home_path(self):
        self.assertCodeIn("rewrite_local_registry_config(&home)?", self.agent_main)
        self.assertCodeIn("pub fn rewrite_local_registry_config(cargo_home: &std::path::Path)", self.agent)
        self.assertCodeIn('let local_registry = cargo_home.join("local-registry")', self.agent)
        self.assertCodeIn("local_registry_config_is_rewritten_to_absolute_guest_cargo_home", self.agent)

    def test_restricted_token_and_access_check_diagnostics_are_removed(self):
        self.assertFalse((WINDOWS / "trusted_prep/launch").exists())
        self.assertFalse((WINDOWS / "trusted_prep/lowbox.rs").exists())
        self.assertFalse((HERE / "test_prep_access_checks.py").exists())
        for source in (self.prepare, self.prep, self.hyperv, self.qualify):
            self.assertNotIn("PrepLaunchMatrix", source)
            self.assertNotIn("AccessChecksOnly", source)
            self.assertNotIn("CreateRestrictedToken", source)

    def test_local_registry_proof_logs_durable_phases_and_skips_extra_parent_validation(self):
        local_registry_entry = self.prep.split("pub(super) fn local_registry_entry", 1)[1]
        self.assertCodeIn('"prep-local-registry-{id}.progress.jsonl"', local_registry_entry)
        self.assertCodeIn('log_phase(&progress_path, "diagnostic-start")?', local_registry_entry)
        self.assertLess(
            local_registry_entry.index('log_phase(&progress_path, "diagnostic-start")?'),
            local_registry_entry.index("system::require_system()?"),
        )
        self.assertLess(
            local_registry_entry.index("system::write_diagnostic_update(&report_path, &report)?"),
            local_registry_entry.index("system::require_system()?"),
        )
        self.assertCodeIn('log_phase(&progress_path, "wait-for-preparation-lock")?', local_registry_entry)
        self.assertCodeIn("VmConfig::load(vm_config_path)?", local_registry_entry)
        self.assertNotIn("Runner::open", local_registry_entry)
        self.assertCodeIn("file.sync_all()", self.prep)
        self.assertCodeIn('"timestamp_unix_ms": timestamp_unix_ms', self.prep)
        self.assertCodeIn("lock_exclusive_bounded(&lock, \"local-registry proof prep.lock\")?", self.prep)
        self.assertCodeIn('timed out waiting 30 seconds for {operation}', self.prep)
        self.assertCodeIn('log_phase(progress_path, "first-job-registry-preparation")?', self.registry)
        self.assertCodeIn('log_phase(progress_path, "run cargo fetch --locked --offline")?', self.registry)
        self.assertCodeIn('log_phase(progress_path, "run cargo build --locked --offline")?', self.registry)
        self.assertCodeIn('phase("download crates.io config https://index.crates.io/config.json")?', self.registry)
        self.assertCodeIn('phase(&format!("download locked crate archive {filename}: https://static.crates.io{path}"))?', self.registry)
        self.assertCodeIn('phase(&format!("{url} read response body"))?', self.registry)

    def test_registry_http_requests_have_total_deadlines_and_named_errors(self):
        self.assertCodeIn("INDEX_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30)", self.registry)
        self.assertCodeIn("ARCHIVE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90)", self.registry)
        self.assertCodeIn("WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 5_000)", self.registry)
        self.assertCodeIn("check_download_deadline(deadline, timeout, host, path", self.registry)
        self.assertCodeIn("local-registry HTTPS request timed out after", self.registry)
        self.assertCodeIn('download_error(host, path, "read response body", error)', self.registry)

    def test_local_registry_proof_checks_pinned_cargo_offline(self):
        self.assertCodeIn("[switch]$LocalRegistryOnly", self.wrapper)
        self.assertCodeIn("Runner-Check 'qualify-prep-local-registry'", self.qualify)
        self.assertCodeIn('"qualify-prep-local-registry" => motor_runner::hyperv_qualify_prep_local_registry(config)', self.main)
        self.assertCodeIn("trusted_prep::local_registry_entry(config)", self.windows)
        for fragment in (
            '"index.crates.io", "/config.json"',
            'download_allowlisted("index.crates.io", &path',
            'download_allowlisted("static.crates.io", &path',
            'local_registry.join("index/cf/g-/cfg-if")',
            "WINHTTP_OPTION_REDIRECT_POLICY_NEVER",
            "MAX_CRATE_INDEX_BYTES: usize = 16 * 1024 * 1024",
            "MAX_CRATE_ARCHIVE_BYTES: usize = 512 * 1024 * 1024",
            "fn write_verified_archive",
            "verify_locked_index_records(&bytes, &name, &locked)?",
            'args([operation, "--locked", "--offline", "--manifest-path"])',
            'run_cargo(cargo, rustc, fixture, &second_home, &target, &temp, "build")',
            "lock_unchanged_after_fetch",
            "lock_unchanged_after_build",
            "cargo_rejected_changed_archive_checksum",
            "repository_source_override_test",
            '"diagnostic_error_mentions_cfg_if": override_error_mentions_cfg_if',
        ):
            self.assertCodeIn(fragment, self.registry)
        self.assertCodeIn("super::super::prep::local_registry_source_config(local_registry, source_name)", self.registry)
        self.assertCodeIn(
            "written_source_path_is_absolute_and_names_the_registry_directory",
            self.prepare,
        )
        self.assertCodeIn('"source_config_path": local_registry', self.registry)
        self.assertCodeIn('"source_config_is_absolute": local_registry.is_absolute()', self.registry)
        self.assertCodeIn('write_job_cargo_config(cargo_home, local_registry.as_deref())?', self.registry)
        self.assertCodeIn('write_job_cargo_config(&corrupt_home, Some(&corrupt_registry))?', self.registry)
        archive_gate = self.registry.split("fn write_verified_archive(", 1)[1].split("\n}", 1)[0]
        self.assertCodeIn(
            'report["repository_source_override_test"]["diagnostic_error_mentions_cfg_if"] == true',
            self.registry,
        )
        self.assertLess(
            archive_gate.index("verify_archive_checksum(bytes, expected_checksum)?"),
            archive_gate.index("OpenOptions::new"),
        )


if __name__ == "__main__":
    unittest.main()
