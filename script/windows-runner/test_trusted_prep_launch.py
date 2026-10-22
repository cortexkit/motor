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
            'local-registry = \\\"{path}\\\"',
            'args([operation, "--locked", "--offline", "--manifest-path"])',
            'run_cargo(cargo, rustc, fixture, &second_home, &target, &temp, "build")',
            "lock_unchanged_after_fetch",
            "lock_unchanged_after_build",
            "cargo_rejected_changed_archive_checksum",
            "repository_source_override_test",
        ):
            self.assertCodeIn(fragment, self.registry)
        archive_gate = self.registry.split("fn write_verified_archive(", 1)[1].split("\n}", 1)[0]
        self.assertLess(
            archive_gate.index("verify_archive_checksum(bytes, expected_checksum)?"),
            archive_gate.index("OpenOptions::new"),
        )


if __name__ == "__main__":
    unittest.main()
