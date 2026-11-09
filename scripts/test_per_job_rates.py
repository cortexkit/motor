#!/usr/bin/env python3
"""Fixture and CLI tests for per-job rate evidence export and comparison."""

from __future__ import annotations

import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent


def load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {filename}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


rates = load("per_job_rates", "per-job-rates.py")
exporter = load("export_per_job_rates", "export-per-job-rates.py")


class PerJobRatesTests(unittest.TestCase):
    def setUp(self):
        self.fixture = json.loads((SCRIPTS / "per-job-rates-fixture.json").read_text())
        self.bundle = exporter.export_bundle(
            self.fixture["config"], self.fixture["journals"],
            self.fixture["accepted_usage"], self.fixture["max_gap_ms"],
        )

    def test_fixture_pins_all_three_rates_and_quarantine_separately(self):
        report = rates.compare(self.bundle)
        tenant, per_job = report["hosts"]["tenant"], report["hosts"]["per_job"]
        self.assertEqual(tenant["denominator"], 5)
        self.assertEqual(per_job["denominator"], 8)
        self.assertEqual(tenant["rates"], {
            "lost": {"count": 1, "rate": 0.2},
            "host_stop": {"count": 1, "rate": 0.2},
            "hosted_host_full": {"count": 1, "rate": 0.2},
            "quarantined_workspace": {"count": 0, "rate": 0.0},
        })
        self.assertEqual(per_job["rates"], {
            "lost": {"count": 2, "rate": 0.25},
            "host_stop": {"count": 2, "rate": 0.25},
            "hosted_host_full": {"count": 1, "rate": 0.125},
            "quarantined_workspace": {"count": 1, "rate": 0.125},
        })
        self.assertEqual(per_job["accepted_usage_jtis"], 6)
        self.assertTrue(report["passed"])
        self.assertEqual(report["coverage_policy"]["max_gap_ms"], 2000)
        self.assertIn("not proof", report["coverage_policy"]["limitation"])

    def test_owner_tolerances_are_absolute_and_equality_passes(self):
        for metric in ("lost", "host_stop"):
            with self.subTest(metric=metric):
                bundle = copy.deepcopy(self.bundle)
                bundle["tolerances"][metric] = 0.049
                report = rates.compare(bundle)
                self.assertFalse(report["passed"])
                self.assertFalse(report["comparisons"][metric]["passed"])
        host = self.bundle["hosts"][1]
        for job in ("p-ok1", "p-ok2"):
            host["claims"].append({"at_ms": 8000, "jti": job, "mode": "per_job",
                                   "result": "refused", "reason": "host_full", "cause": "permits"})
        self.bundle["tolerances"]["hosted_host_full"] = 0.175
        self.assertTrue(rates.compare(self.bundle)["passed"])
        self.bundle["tolerances"]["hosted_host_full"] = 0.174
        self.assertFalse(rates.compare(self.bundle)["passed"])

    def test_canaries_are_removed_from_every_numerator_and_denominator(self):
        self.bundle["canary_jtis"] = []
        host = rates.compare(self.bundle)["hosts"]["per_job"]
        self.assertEqual(host["denominator"], 9)
        for metric, expected in (("lost", 3), ("host_stop", 3), ("hosted_host_full", 2)):
            self.assertEqual(host["rates"][metric]["count"], expected)

    def test_retries_usage_resends_and_stop_duplicates_count_once(self):
        host = self.bundle["hosts"][1]
        for key in ("claims", "accepted_usage", "host_stops"):
            host[key] *= 3
        self.assertEqual(rates.compare(self.bundle), rates.compare(exporter.export_bundle(
            self.fixture["config"], self.fixture["journals"],
            self.fixture["accepted_usage"], self.fixture["max_gap_ms"],
        )))

    def test_only_broker_usage_for_claimed_cohort_supplies_usage_numerators(self):
        host = self.bundle["hosts"][1]
        host["accepted_usage"] = [body for body in host["accepted_usage"]
                                  if body["job_id"] != "p-lost"]
        report = rates.compare(self.bundle)["hosts"]["per_job"]
        self.assertEqual(report["denominator"], 8)
        self.assertEqual(report["rates"]["lost"]["count"], 1)
        self.assertEqual(report["accepted_usage_jtis"], 5)

    def test_host_stop_before_started_counts_even_without_a_usage_report(self):
        host = self.bundle["hosts"][1]
        host["accepted_usage"] = [body for body in host["accepted_usage"]
                                  if body["job_id"] != "p-stop-before-started"]
        report = rates.compare(self.bundle)["hosts"]["per_job"]
        self.assertEqual(report["rates"]["host_stop"]["count"], 2)
        self.assertEqual(report["rates"]["lost"]["count"], 1)

    def test_quarantine_does_not_hide_a_separate_capacity_refusal(self):
        self.bundle["hosts"][1]["claims"].append({
            "at_ms": 6000, "jti": "p-quarantine", "mode": "per_job", "result": "refused",
            "reason": "host_full", "cause": "memory",
        })
        report = rates.compare(self.bundle)["hosts"]["per_job"]["rates"]
        self.assertEqual(report["hosted_host_full"]["count"], 2)
        self.assertEqual(report["quarantined_workspace"]["count"], 1)

    def test_non_host_full_refusals_are_only_in_denominator(self):
        self.bundle["hosts"][1]["claims"].append({
            "at_ms": 6000, "jti": "p-expired", "mode": "per_job", "result": "refused",
            "reason": "expired", "cause": "expired",
        })
        host = rates.compare(self.bundle)["hosts"]["per_job"]
        self.assertEqual(host["denominator"], 9)
        self.assertEqual(host["rates"]["hosted_host_full"]["count"], 1)

    def test_conflicting_accepted_usage_is_not_silently_combined(self):
        usage = copy.deepcopy(self.bundle["hosts"][1]["accepted_usage"][0])
        usage["outcome"] = "exit"
        self.bundle["hosts"][1]["accepted_usage"].append(usage)
        with self.assertRaisesRegex(ValueError, "conflicting broker-accepted usage"):
            rates.compare(self.bundle)

    def test_both_hosts_require_coverage_at_both_window_edges(self):
        for mode in ("tenant", "per_job"):
            for edge in ("start_ms", "end_ms"):
                with self.subTest(mode=mode, edge=edge):
                    bundle = copy.deepcopy(self.bundle)
                    host = next(item for item in bundle["hosts"] if item["mode"] == mode)
                    host["coverage"] = [{"start_ms": 1000, "end_ms": 11000}]
                    host["coverage"][0][edge] += 1 if edge == "start_ms" else -1
                    with self.assertRaisesRegex(ValueError, "log does not cover the whole window"):
                        rates.compare(bundle)

    def test_exported_journal_gap_causes_comparator_refusal(self):
        journals = copy.deepcopy(self.fixture["journals"])
        journals["per_job"] = self.fixture["gapped_per_job_journal"]
        bundle = exporter.export_bundle(self.fixture["config"], journals,
                                        self.fixture["accepted_usage"], 2000)
        self.assertEqual([(item["start_ms"], item["end_ms"])
                          for item in bundle["hosts"][1]["coverage"]],
                         [(1000, 1000), (7000, 11000)])
        with self.assertRaisesRegex(ValueError, r"host-per-job.*\[\[1000, 7000\]\]"):
            rates.compare(bundle)

    def test_boot_and_journal_sequence_changes_split_coverage(self):
        for field, replacement in (("_BOOT_ID", "new-boot"), ("__CURSOR", "s=rotated;i=1")):
            with self.subTest(field=field):
                journals = copy.deepcopy(self.fixture["journals"])
                journals["tenant"][-1][field] = replacement
                bundle = exporter.export_bundle(self.fixture["config"], journals,
                                                self.fixture["accepted_usage"], 2000)
                self.assertEqual(len(bundle["hosts"][0]["coverage"]), 2)
                with self.assertRaisesRegex(ValueError, "log does not cover"):
                    rates.compare(bundle)

    def test_exporter_never_extrapolates_or_rounds_outward(self):
        rows = copy.deepcopy(self.fixture["journals"]["tenant"])
        rows[0]["__REALTIME_TIMESTAMP"] = "1000001"
        coverage = exporter.journal_export(rows, 2000)["coverage"]
        self.assertEqual((coverage[0]["start_ms"], coverage[0]["end_ms"]), (1001, 11000))
        self.assertEqual(exporter.journal_export([], 2000)["coverage"], [])
        coverage = exporter.journal_export(rows[1:-1], 2000)["coverage"]
        self.assertEqual((coverage[0]["start_ms"], coverage[0]["end_ms"]), (2000, 9000))

    def test_invalid_journal_identity_and_time_are_rejected(self):
        for field, value in (("__CURSOR", "i=1"), ("__REALTIME_TIMESTAMP", "-1"),
                             ("_HOSTNAME", "other-host"), ("_SYSTEMD_UNIT", "other.service")):
            with self.subTest(field=field):
                rows = copy.deepcopy(self.fixture["journals"]["tenant"])
                rows[-1][field] = value
                with self.assertRaises(ValueError):
                    exporter.journal_export(rows, 2000)
        rows = list(reversed(self.fixture["journals"]["tenant"]))
        with self.assertRaisesRegex(ValueError, "time goes backwards"):
            exporter.journal_export(rows, 2000)

    def test_event_names_inside_unrelated_messages_are_not_claims(self):
        row = copy.deepcopy(self.fixture["journals"]["tenant"][0])
        row["MESSAGE"] = (
            "[motor-host] INFO motor_host::server: diagnostic: run_claim "
            "jti=decoy mode=tenant result=admitted"
        )
        self.assertEqual(exporter.journal_export([row], 2000)["claims"], [])

    def test_missing_tolerances_empty_cohorts_and_invalid_gap_are_rejected(self):
        del self.bundle["tolerances"]["lost"]
        with self.assertRaises(KeyError):
            rates.compare(self.bundle)
        self.bundle["hosts"][0]["claims"] = []
        with self.assertRaisesRegex(ValueError, "no non-canary"):
            rates.compare(self.bundle)
        for gap in (0, -1, True):
            with self.assertRaises(ValueError):
                exporter.journal_export([], gap)

    def test_cli_export_compare_and_no_report_on_incomplete_logs(self):
        scratch = Path(os.environ.get("TMPDIR", SCRIPTS.parent / "target/scratch"))
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="per-job-rates-", dir=scratch) as directory:
            root = Path(directory)
            config = root / "config.json"
            config.write_text(json.dumps(self.fixture["config"]))
            args = [sys.executable, str(SCRIPTS / "export-per-job-rates.py"), "--config", str(config)]
            for mode in ("tenant", "per_job"):
                journal, usage = root / f"{mode}.jsonl", root / f"{mode}-usage.json"
                journal.write_text("\n".join(json.dumps(row) for row in self.fixture["journals"][mode]))
                usage.write_text(json.dumps(self.fixture["accepted_usage"][mode]))
                args.extend([f"--{mode.replace('_', '-')}-journal", str(journal),
                             f"--{mode.replace('_', '-')}-usage", str(usage)])
            missing = subprocess.run(args, capture_output=True, text=True)
            self.assertEqual(missing.returncode, 2)
            self.assertIn("--max-gap-ms", missing.stderr)
            args.extend(["--max-gap-ms", "2000"])
            exported = subprocess.run(args, capture_output=True, text=True)
            self.assertEqual(exported.returncode, 0, exported.stderr)
            bundle = root / "bundle.json"
            bundle.write_text(exported.stdout)
            compare_args = [sys.executable, str(SCRIPTS / "per-job-rates.py"), str(bundle)]
            result = subprocess.run(compare_args, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(json.loads(result.stdout)["passed"])
            failed = json.loads(exported.stdout)
            failed["tolerances"]["lost"] = 0
            bundle.write_text(json.dumps(failed))
            result = subprocess.run(compare_args, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertFalse(json.loads(result.stdout)["passed"])
            (root / "per_job.jsonl").write_text("\n".join(
                json.dumps(row) for row in self.fixture["gapped_per_job_journal"]))
            exported = subprocess.run(args, capture_output=True, text=True)
            self.assertEqual(exported.returncode, 0, exported.stderr)
            bundle.write_text(exported.stdout)
            result = subprocess.run(compare_args, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, "")
            self.assertIn("uncovered ms: [[1000, 7000]]", result.stderr)
            self.assertIn("max_gap_ms=2000", result.stderr)
            self.assertIn("not proof", result.stderr)


if __name__ == "__main__":
    unittest.main()
