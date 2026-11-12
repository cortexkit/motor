"""Tests for remote-cache-measure.py. Run: python3 -m unittest script/test_remote_cache_measure.py

Every input is a small fixture written the way its producer writes it:
motor-host journal exports, runner job records under ``jobs/<id>/record.json``,
and ck-motor's ``state.json`` checkpoint with journal segments.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
# The script's name has hyphens, so it loads from its file, not by import.
SPEC = importlib.util.spec_from_file_location("remote_cache_measure", HERE / "remote-cache-measure.py")
measure = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(measure)

MOTOR = "/home/dev/src/motor"
WEB = "/home/dev/src/web"
REPOSITORIES = {MOTOR: "motor", WEB: "web"}
# 2026-11-10T23:30:00Z and the following midnight, in epoch milliseconds.
EVENING_MS = 1794353400000
MIDNIGHT_MS = 1794355200000


def job_id(n: int) -> str:
    return f"0190a0b0-0000-7000-8000-{n:012d}"


def totals_line(when: str, n: int, hits: int, misses: int, **puts: int) -> str:
    fields = " ".join(f"{k}={v}" for k, v in puts.items())
    return (
        f"{when} ovh-1 motor-host[812]: [motor-host] INFO motor_host::state: "
        f"job remote cache totals (guest-reported) job_id={job_id(n)} "
        f"hits={hits} misses={misses} errors=0 {fields}"
    ).rstrip()


def ended_line(when: str, n: int) -> str:
    return (
        f"{when} ovh-1 motor-host[812]: [motor-host] INFO motor_host::state: "
        f"job memory peaks (guest-reported) job_id={job_id(n)} vcpus=4 "
        "mem_working_peak_mib=Some(100) mem_peak_mib=Some(120)"
    )


def runner_record(n: int, finished_ms: int, totals: dict | None, state: str = "Terminal",
                  workspace_key: str | None = None) -> dict:
    record = {"id": job_id(n), "state": state, "finished_ms": finished_ms,
              "accepted_ms": finished_ms - 60_000, "base": False,
              "request": None if workspace_key is None else {"workspace_key": workspace_key}}
    if totals is not None:
        record["remote_cache_totals"] = totals
    return record


class Fixture:
    """A scratch directory holding one set of inputs."""

    def __init__(self, root: Path):
        self.root = root
        self.host_log = root / "motor-host.log"
        self.runner_state = root / "runner-state"
        self.ck_motor = root / "ck-motor"
        self.repositories = root / "repositories.json"
        (self.runner_state / "jobs").mkdir(parents=True)
        self.ck_motor.mkdir()
        self.repositories.write_text(json.dumps(REPOSITORIES))
        self.host_log.write_text("")

    def log(self, *lines: str) -> None:
        with self.host_log.open("a") as out:
            out.write("".join(line + "\n" for line in lines))

    def record(self, record: dict) -> None:
        directory = self.runner_state / "jobs" / record["id"]
        directory.mkdir()
        (directory / "record.json").write_text(json.dumps(record))

    def caller_state(self, jobs: dict[int, dict]) -> None:
        state = {"jobs": {job_id(n): job for n, job in jobs.items()}, "snapshots": {}}
        (self.ck_motor / "state.json").write_text(json.dumps(state))

    def caller_journal(self, first: int, frames: list[dict[int, dict | None]], tail: str = "") -> None:
        lines = [
            json.dumps({"epoch": "0190a0b0-0000-7000-8000-00000000ffff", "seq": first + i,
                        "delta": {"jobs": {job_id(n): job for n, job in frame.items()}}})
            for i, frame in enumerate(frames)
        ]
        name = f"journal-0190a0b0-0000-7000-8000-00000000ffff-{first:020}.jsonl"
        (self.ck_motor / name).write_text("\n".join(lines) + "\n" + tail)

    def report(self, *extra: str) -> dict:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = measure.main([
                "--host-log", str(self.host_log),
                "--runner-jobs", str(self.runner_state),
                "--ck-motor", str(self.ck_motor),
                "--repositories", str(self.repositories),
                "--format", "json", *extra,
            ])
        self.assert_ok(code)
        return json.loads(out.getvalue())

    def text(self, *extra: str) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = measure.main([
                "--host-log", str(self.host_log),
                "--runner-jobs", str(self.runner_state),
                "--ck-motor", str(self.ck_motor),
                "--repositories", str(self.repositories), *extra,
            ])
        self.assert_ok(code)
        return out.getvalue()

    @staticmethod
    def assert_ok(code: int) -> None:
        if code != 0:
            raise AssertionError(f"measure exited {code}")


def caller(workspace_key: str | None, **extra) -> dict:
    return {"queue_wait_limit_s": 60, "pending_cancel": False, "local_terminal": None,
            "workspace_key": workspace_key, **extra}


def row(report: dict, repository: str, day: str) -> dict:
    matches = [r for r in report["rows"] if r["repository"] == repository and r["day"] == day]
    if len(matches) != 1:
        raise AssertionError(f"expected one {repository} {day} row, got {report['rows']}")
    return matches[0]


class MeasureTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.f = Fixture(Path(self._tmp.name))

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_overlapping_jobs_use_their_own_totals_and_the_day_each_ended(self) -> None:
        # Jobs 1 and 2 ran at the same time; job 2 ended after midnight. The
        # runner's gateway-wide running counts, logged at each job end, must
        # not be attributed to either job.
        self.f.caller_state({1: caller(MOTOR), 2: caller(MOTOR + "/crates/motor-host"),
                             3: caller(MOTOR)})
        self.f.log(
            "2026-11-10T23:40:00+0000 ovh-1 motor-runner[9]: [motor-runner] INFO "
            "motor_runner::engine: remote cache totals at job end job_id=" + job_id(1)
            + " tool=sccache hits=900 misses=900 probes=0 errors=0",
            totals_line("2026-11-10T23:40:00+0000", 1, 30, 10, puts_accepted=10,
                        put_bytes_accepted=4096),
            totals_line("2026-11-11T00:05:00.250000+00:00", 2, 5, 15, puts_accepted=15,
                        puts_duplicate=2, puts_failed=1, puts_refused=3,
                        put_bytes_accepted=1000),
        )
        # A third overlapping job is known only from its runner record.
        self.f.record(runner_record(3, EVENING_MS + 20 * 60_000,
                                    {"hits": 10, "misses": 0, "errors": 0}))
        report = self.f.report()

        evening = row(report, "motor", "2026-11-10")
        self.assertEqual(evening["jobs"], 2)
        self.assertEqual(evening["jobs_with_totals"], 2)
        self.assertEqual((evening["hits"], evening["misses"]), (40, 10))
        self.assertAlmostEqual(evening["aggregate_hit_rate"], 0.8)
        self.assertAlmostEqual(evening["median_job_hit_rate"], (0.75 + 1.0) / 2)
        self.assertEqual(evening["puts_accepted"], 10)
        self.assertEqual(evening["put_bytes_accepted"], 4096)

        after_midnight = row(report, "motor", "2026-11-11")
        self.assertEqual(after_midnight["jobs"], 1)
        self.assertEqual((after_midnight["hits"], after_midnight["misses"]), (5, 15))
        self.assertAlmostEqual(after_midnight["aggregate_hit_rate"], 0.25)
        self.assertEqual(
            [after_midnight[k] for k in ("puts_accepted", "puts_duplicate", "puts_failed",
                                         "puts_refused", "put_bytes_accepted")],
            [15, 2, 1, 3, 1000],
        )
        self.assertEqual(len(report["rows"]), 2)

    def test_repeated_records_for_one_job_count_once_on_its_first_end(self) -> None:
        self.f.caller_state({1: caller(MOTOR)})
        totals = {"hits": 6, "misses": 2, "errors": 0, "puts_accepted": 2,
                  "put_bytes_accepted": 50}
        # The runner record, the live host line, and a line replayed after a
        # host restart the next day all describe the same job.
        self.f.record(runner_record(1, EVENING_MS, totals))
        self.f.log(
            totals_line("2026-11-10T23:30:01+0000", 1, 6, 2, puts_accepted=2,
                        put_bytes_accepted=50),
            ended_line("2026-11-10T23:30:01+0000", 1),
            totals_line("2026-11-11T09:00:00+0000", 1, 6, 2, puts_accepted=2,
                        put_bytes_accepted=50),
        )
        report = self.f.report()
        self.assertEqual(len(report["rows"]), 1)
        only = row(report, "motor", "2026-11-10")
        self.assertEqual((only["jobs"], only["jobs_with_totals"]), (1, 1))
        self.assertEqual((only["hits"], only["misses"], only["puts_accepted"],
                          only["put_bytes_accepted"]), (6, 2, 2, 50))

    def test_a_job_without_totals_is_missing_never_zero(self) -> None:
        self.f.caller_state({1: caller(WEB), 2: caller(WEB), 3: caller(WEB), 4: caller(WEB)})
        self.f.log(
            totals_line("2026-11-10T10:00:00+0000", 1, 9, 1),
            # Job 2 ended on the host without totals.
            ended_line("2026-11-10T11:00:00+0000", 2),
        )
        # Job 3 is a build-server record with no totals; job 4 never ended and
        # does not count at all.
        self.f.record(runner_record(3, EVENING_MS, None))
        self.f.record(runner_record(4, EVENING_MS, None, state="Running"))
        report = self.f.report()
        web = row(report, "web", "2026-11-10")
        self.assertEqual(web["jobs"], 3)
        self.assertEqual(web["jobs_with_totals"], 1)
        self.assertEqual(web["jobs_without_totals"], 2)
        # Missing jobs would pull these down if they were counted as zero.
        self.assertEqual((web["hits"], web["misses"]), (9, 1))
        self.assertAlmostEqual(web["median_job_hit_rate"], 0.9)
        self.assertAlmostEqual(web["aggregate_hit_rate"], 0.9)

    def test_a_row_with_no_totals_leaves_counts_and_rates_empty(self) -> None:
        self.f.caller_state({1: caller(WEB)})
        self.f.log(ended_line("2026-11-10T11:00:00+0000", 1))
        web = row(self.f.report(), "web", "2026-11-10")
        self.assertEqual((web["jobs"], web["jobs_with_totals"], web["jobs_without_totals"]),
                         (1, 0, 1))
        for name in ("hits", "misses", "median_job_hit_rate", "aggregate_hit_rate",
                     "puts_accepted", "puts_duplicate", "puts_failed", "puts_refused",
                     "put_bytes_accepted"):
            self.assertIsNone(web[name], name)
        line = [l for l in self.f.text().splitlines() if l.startswith("web\t")][0]
        self.assertEqual(line.split("\t"), ["web", "2026-11-10", "1", "0", "1"] + [""] * 9)

    def test_zero_denominators_print_empty(self) -> None:
        # Both jobs have totals but made no lookups at all.
        self.f.caller_state({1: caller(MOTOR), 2: caller(MOTOR)})
        self.f.log(totals_line("2026-11-10T10:00:00+0000", 1, 0, 0),
                   totals_line("2026-11-10T10:05:00+0000", 2, 0, 0, puts_refused=4))
        report = self.f.report()
        motor = row(report, "motor", "2026-11-10")
        self.assertEqual((motor["jobs_with_totals"], motor["hits"], motor["misses"]), (2, 0, 0))
        self.assertIsNone(motor["median_job_hit_rate"])
        self.assertIsNone(motor["aggregate_hit_rate"])
        self.assertEqual(motor["puts_refused"], 4)
        line = [l for l in self.f.text().splitlines() if l.startswith("motor\t")][0]
        cells = line.split("\t")
        columns = ("repository", "day") + measure.COLUMNS
        self.assertEqual(cells[columns.index("median_job_hit_rate")], "")
        self.assertEqual(cells[columns.index("aggregate_hit_rate")], "")
        self.assertEqual(cells[columns.index("hits")], "0")
        self.assertEqual(cells[columns.index("puts_refused")], "4")

    def test_jobs_without_a_workspace_mapping_go_to_the_unmapped_row(self) -> None:
        # Job 1 is mapped. Job 2's workspace is outside every mapped path, and
        # a sibling directory sharing a name prefix is not inside the map.
        # Job 3 is unknown to ck-motor and its runner record names no
        # workspace. Job 4 has a record whose key ck-motor lost.
        self.f.caller_state({1: caller(MOTOR), 2: caller(MOTOR + "-old"), 4: caller(None)})
        for n, hits in ((1, 1), (2, 2), (3, 3), (4, 4)):
            self.f.log(totals_line("2026-11-10T10:00:00+0000", n, hits, 1))
        report = self.f.report()
        self.assertEqual([(r["repository"], r["jobs"]) for r in report["rows"]],
                         [("motor", 1), ("unmapped", 3)])
        unmapped = row(report, "unmapped", "2026-11-10")
        self.assertEqual((unmapped["hits"], unmapped["misses"]), (9, 3))
        text = self.f.text()
        self.assertTrue(text.splitlines()[-1].startswith("unmapped\t2026-11-10\t3\t"))

    def test_runner_request_workspace_names_a_job_ck_motor_does_not_know(self) -> None:
        self.f.record(runner_record(5, EVENING_MS, {"hits": 1, "misses": 1, "errors": 0},
                                    workspace_key=WEB))
        self.assertEqual(row(self.f.report(), "web", "2026-11-10")["jobs"], 1)

    def test_ck_motor_journal_and_resubmitted_build_server_id(self) -> None:
        resubmitted = job_id(99)
        self.f.caller_state({1: caller(None)})
        # The journal names job 1's workspace later, then prunes it; job 2 was
        # resubmitted to the build server, which reports it under its own id.
        self.f.caller_journal(7, [{1: caller(WEB)}, {1: None},
                                  {2: caller(MOTOR, build_server_job_id=resubmitted)}],
                              tail='{"epoch": "cut short')
        self.f.log(totals_line("2026-11-10T10:00:00+0000", 1, 1, 0))
        self.f.log(totals_line("2026-11-10T10:00:00+0000", 99, 2, 0))
        report = self.f.report()
        self.assertEqual(row(report, "web", "2026-11-10")["hits"], 1)
        self.assertEqual(row(report, "motor", "2026-11-10")["hits"], 2)

    def test_journal_json_export_and_ck_motor_terminal_time_fallback(self) -> None:
        self.f.caller_state({1: caller(MOTOR), 2: caller(MOTOR, terminal_observed_at_ms=MIDNIGHT_MS)})
        message = ("[motor-host] INFO motor_host::state: job remote cache totals (guest-reported) "
                   f"job_id={job_id(1)} hits=3 misses=1 errors=0")
        self.f.log(json.dumps({"__REALTIME_TIMESTAMP": str(EVENING_MS * 1000), "MESSAGE": message}),
                   # A text line without an ISO time takes ck-motor's terminal time.
                   "Nov 11 00:00:01 ovh-1 motor-host[812]: [motor-host] INFO motor_host::state: "
                   f"job remote cache totals (guest-reported) job_id={job_id(2)} hits=1 misses=0 errors=0")
        report = self.f.report()
        self.assertEqual(row(report, "motor", "2026-11-10")["hits"], 3)
        self.assertEqual(row(report, "motor", "2026-11-11")["hits"], 1)

    def test_report_states_the_combined_basis_and_compares_before_and_after(self) -> None:
        self.f.caller_state({1: caller(MOTOR), 2: caller(MOTOR), 3: caller(MOTOR)})
        self.f.log(totals_line("2026-11-09T10:00:00+0000", 1, 1, 3),
                   totals_line("2026-11-10T10:00:00+0000", 2, 3, 1),
                   ended_line("2026-11-11T10:00:00+0000", 3))
        report = self.f.report("--cutover", "2026-11-10")
        self.assertIn("combine sccache and ccache", report["basis"])
        comparison = {r["period"]: r for r in report["comparison"]}
        self.assertEqual(list(comparison), ["before", "after"])
        self.assertAlmostEqual(comparison["before"]["aggregate_hit_rate"], 0.25)
        self.assertAlmostEqual(comparison["after"]["aggregate_hit_rate"], 0.75)
        self.assertEqual(comparison["after"]["jobs_without_totals"], 1)
        text = self.f.text("--cutover", "2026-11-10")
        self.assertIn("combine sccache and ccache", text)
        self.assertIn("same combined sccache and ccache basis", text)
        self.assertIn("motor\tafter\t2\t1\t1\t3\t1\t0.7500\t0.7500\t", text)

    def test_repository_map_cannot_claim_the_unmapped_row(self) -> None:
        self.f.repositories.write_text(json.dumps({MOTOR: "unmapped"}))
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            self.f.report()


if __name__ == "__main__":
    unittest.main()
