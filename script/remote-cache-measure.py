#!/usr/bin/env python3
"""Measure the remote compile cache per repository and per UTC day.

Usage:

    script/remote-cache-measure.py --repositories paths.json \\
        [--host-log motor-host.log ...] [--runner-jobs STATE_OR_JOBS_DIR ...] \\
        [--ck-motor CK_MOTOR_STORAGE_DIR ...] [--cutover YYYY-MM-DD] \\
        [--format text|json]

Inputs, each option repeatable:

- ``--host-log``: motor-host's log as exported from the journal, either
  ``journalctl -o short-iso`` (or ``short-iso-precise``) text or
  ``journalctl -o json`` lines. Each terminal job logs one line "job remote
  cache totals (guest-reported)" with its job id and totals. A terminal job
  without totals still logs "job memory peaks (guest-reported)", so that line
  counts the job as ended without totals. The runner's own "remote cache totals
  at job end" lines are gateway-wide running counts, not per-job counts, and are
  ignored.
- ``--runner-jobs``: a runner's job records: a ``record.json`` file, one job
  directory, a ``jobs`` directory, or the runner state directory above it.
  Records that are not ended (``Terminal`` or ``Lost``) are skipped, and so
  are the runner's own build-base jobs (``"base": true``), which warm the
  cache for later jobs and belong to no caller.
- ``--ck-motor``: ck-motor's storage: its directory, its ``state.json``
  checkpoint or one ``journal-*.jsonl`` segment. It supplies the
  ``workspace_key`` it retains for each job, including the build-server job id
  of a resubmitted job.
- ``--repositories``: a JSON object mapping a local checkout path to a
  repository name, for example ``{"/home/me/src/motor": "motor"}``. A workspace
  key matches the longest path that equals it or contains it.

Jobs are joined by job id, then by workspace key. Each job id counts once, on
the UTC day it ended. Only the totals sources (host log and runner records)
decide which jobs exist; ck-motor and the runner's own copy of the request
only name the workspace.

Hit rates use the existing per-job totals, which combine sccache and ccache
lookups. A job without totals counts as missing, never as zero hits. A job
whose workspace key is unknown or matches no path goes to the ``unmapped``
row. A rate whose denominator is zero prints empty.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import statistics
import sys
from pathlib import Path
from typing import Iterable, Iterator

UNMAPPED = "unmapped"
UNKNOWN_DAY = "unknown"
BASIS = (
    "Hit rates use the existing per-job totals, which combine sccache and "
    "ccache lookups; they are not sccache-only. Before and after are compared "
    "on that same combined basis. A job without totals counts as missing, "
    "never as zero hits, and is left out of hits, misses, rates and PUT "
    "counts; a row with no totals at all leaves those cells empty. An empty "
    "rate marks a zero denominator."
)
# Totals fields as motor-host logs them and as RemoteCacheTotals serializes
# them. A host or runner older than job cache writes omits the PUT fields,
# which then count as zero, exactly as the serialized form omits zeros.
TOTAL_FIELDS = (
    "hits",
    "misses",
    "puts_accepted",
    "puts_duplicate",
    "puts_failed",
    "puts_refused",
    "put_bytes_accepted",
)
COLUMNS = (
    "jobs",
    "jobs_with_totals",
    "jobs_without_totals",
    "hits",
    "misses",
    "median_job_hit_rate",
    "aggregate_hit_rate",
    "puts_accepted",
    "puts_duplicate",
    "puts_failed",
    "puts_refused",
    "put_bytes_accepted",
)
TOTALS_MESSAGE = "job remote cache totals (guest-reported)"
ENDED_MESSAGE = "job memory peaks (guest-reported)"
ENDED_STATES = {"Terminal", "Lost"}
UUID = re.compile(r"^[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$")
FIELD = re.compile(r"(\w+)=(\S+)")
OFFSET = re.compile(r"([+-]\d{2})(\d{2})$")


class Record:
    """One observation of one ended job from a totals source."""

    def __init__(
        self,
        job_id: str,
        ended_ms: int | None,
        totals: dict[str, int] | None,
        workspace_key: str | None = None,
    ):
        self.job_id = job_id
        self.ended_ms = ended_ms
        self.totals = totals
        self.workspace_key = workspace_key


class Job:
    """Every observation of one job id, merged."""

    def __init__(self, job_id: str):
        self.job_id = job_id
        self.ended_ms: int | None = None
        self.totals: dict[str, int] | None = None
        self.totals_ended_ms: int | None = None
        self.fallback_workspace_key: str | None = None

    def add(self, record: Record) -> None:
        # A record replayed after a restart carries a later time than the job's
        # real end, so the earliest end wins.
        if record.ended_ms is not None and (
            self.ended_ms is None or record.ended_ms < self.ended_ms
        ):
            self.ended_ms = record.ended_ms
        if record.totals is not None and (
            self.totals is None
            or (
                record.ended_ms is not None
                and (self.totals_ended_ms is None or record.ended_ms < self.totals_ended_ms)
            )
        ):
            self.totals = record.totals
            self.totals_ended_ms = record.ended_ms
        if self.fallback_workspace_key is None and record.workspace_key:
            self.fallback_workspace_key = record.workspace_key


def canonical_id(value: object) -> str | None:
    if isinstance(value, str) and UUID.match(value):
        return value.lower()
    return None


def count(value: object) -> int:
    """A non-negative integer count, or 0 for anything else."""
    if isinstance(value, bool):
        return 0
    if isinstance(value, int):
        return max(value, 0)
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return 0


def totals_from(mapping: dict) -> dict[str, int]:
    return {name: count(mapping.get(name)) for name in TOTAL_FIELDS}


def parse_timestamp_ms(text: str) -> int | None:
    """Milliseconds since the epoch for an ISO 8601 time; naive means UTC."""
    text = text.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    text = OFFSET.sub(r"\1:\2", text)
    try:
        moment = dt.datetime.fromisoformat(text)
    except ValueError:
        return None
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=dt.timezone.utc)
    return int(moment.timestamp() * 1000)


def host_log_line(line: str) -> tuple[str, int | None] | None:
    """The message and time of one exported journal line.

    ``journalctl -o json`` lines carry the message and a microsecond
    timestamp. Text lines start with the ISO time of the ``short-iso``
    formats; a line without one has no known time.
    """
    line = line.strip()
    if not line:
        return None
    if line.startswith("{"):
        try:
            entry = json.loads(line)
        except ValueError:
            return None
        if not isinstance(entry, dict):
            return None
        message = entry.get("MESSAGE")
        # journalctl writes a message that is not valid UTF-8 as a byte list.
        if isinstance(message, list):
            message = bytes(b & 0xFF for b in message if isinstance(b, int)).decode(
                "utf-8", "replace"
            )
        if not isinstance(message, str):
            return None
        stamp = entry.get("__REALTIME_TIMESTAMP")
        ended = int(stamp) // 1000 if isinstance(stamp, str) and stamp.isdigit() else None
        return message, ended
    first, _, _ = line.partition(" ")
    return line, parse_timestamp_ms(first)


def read_host_log(lines: Iterable[str]) -> Iterator[Record]:
    for line in lines:
        parsed = host_log_line(line)
        if parsed is None:
            continue
        message, ended_ms = parsed
        for marker, has_totals in ((TOTALS_MESSAGE, True), (ENDED_MESSAGE, False)):
            at = message.find(marker)
            if at < 0:
                continue
            fields = dict(FIELD.findall(message[at + len(marker):]))
            job_id = canonical_id(fields.get("job_id"))
            if job_id is None:
                break
            yield Record(job_id, ended_ms, totals_from(fields) if has_totals else None)
            break


def runner_record_paths(path: Path) -> list[Path]:
    if path.is_file():
        return [path]
    if (path / "record.json").is_file():
        return [path / "record.json"]
    # Only direct job directories: a job's returned files live under it and may
    # contain a file of the same name, and incomplete first records wait in a
    # sibling staging directory.
    jobs = path / "jobs" if (path / "jobs").is_dir() else path
    return sorted(p for p in jobs.glob("*/record.json") if p.is_file())


def read_runner_record(value: object) -> Record | None:
    if not isinstance(value, dict) or value.get("base") is True:
        return None
    if value.get("state") not in ENDED_STATES:
        return None
    job_id = canonical_id(value.get("id"))
    if job_id is None:
        return None
    ended = value.get("finished_ms")
    if not isinstance(ended, int) or isinstance(ended, bool):
        ended = value.get("execution_ended_ms")
    ended_ms = ended if isinstance(ended, int) and not isinstance(ended, bool) else None
    raw = value.get("remote_cache_totals")
    totals = totals_from(raw) if isinstance(raw, dict) else None
    request = value.get("request")
    key = request.get("workspace_key") if isinstance(request, dict) else None
    return Record(job_id, ended_ms, totals, key if isinstance(key, str) else None)


def read_runner_jobs(path: Path) -> Iterator[Record]:
    for record_path in runner_record_paths(path):
        try:
            value = json.loads(record_path.read_text())
        except (OSError, ValueError) as error:
            print(f"skipping {record_path}: {error}", file=sys.stderr)
            continue
        record = read_runner_record(value)
        if record is not None:
            yield record


class Workspaces:
    """ck-motor's retained workspace key and terminal time per job id."""

    def __init__(self) -> None:
        self.keys: dict[str, str] = {}
        self.terminal_ms: dict[str, int] = {}

    def add_job(self, job_id: object, value: object) -> None:
        job_id = canonical_id(job_id)
        if job_id is None or not isinstance(value, dict):
            return
        ids = [job_id]
        # A resubmitted job ran on the build server under this other id.
        other = canonical_id(value.get("build_server_job_id"))
        if other is not None:
            ids.append(other)
        key = value.get("workspace_key")
        observed = value.get("terminal_observed_at_ms")
        for one in ids:
            if isinstance(key, str) and key:
                self.keys[one] = key
            if isinstance(observed, int) and not isinstance(observed, bool):
                self.terminal_ms[one] = observed

    def add_state(self, value: object) -> None:
        jobs = value.get("jobs") if isinstance(value, dict) else None
        if isinstance(jobs, dict):
            for job_id, job in jobs.items():
                self.add_job(job_id, job)

    def add_journal(self, lines: Iterable[str]) -> None:
        for line in lines:
            try:
                frame = json.loads(line)
            except ValueError:
                # An append cut short by a crash leaves a partial last line.
                continue
            delta = frame.get("delta") if isinstance(frame, dict) else None
            jobs = delta.get("jobs") if isinstance(delta, dict) else None
            if isinstance(jobs, dict):
                # A removed job (null) keeps the key already seen: removal is
                # retention pruning, not a change of workspace.
                for job_id, job in jobs.items():
                    self.add_job(job_id, job)

    def add_path(self, path: Path) -> None:
        if path.is_dir():
            state = path / "state.json"
            if state.is_file():
                self.add_state(json.loads(state.read_text()))
            # Segments not yet folded into the checkpoint are newer than it.
            for segment in sorted(path.glob("journal-*.jsonl"), key=segment_order):
                self.add_journal(segment.read_text().splitlines())
        elif path.suffix == ".jsonl":
            self.add_journal(path.read_text().splitlines())
        else:
            self.add_state(json.loads(path.read_text()))


def segment_order(path: Path) -> tuple[int, str]:
    first = path.stem.rsplit("-", 1)[-1]
    return (int(first) if first.isdigit() else 0, path.name)


class Repositories:
    """The local map from checkout paths to repository names."""

    def __init__(self, mapping: object):
        if not isinstance(mapping, dict):
            raise ValueError("the repository map must be a JSON object")
        self.paths: list[tuple[str, str]] = []
        for path, name in mapping.items():
            if not isinstance(path, str) or not isinstance(name, str) or not name:
                raise ValueError("the repository map must map paths to names")
            if name == UNMAPPED:
                raise ValueError(f"{UNMAPPED!r} is reserved for jobs with no mapping")
            self.paths.append((normalize(path), name))
        self.paths.sort(key=lambda item: len(item[0]), reverse=True)

    def name(self, workspace_key: str | None) -> str:
        if not workspace_key:
            return UNMAPPED
        key = normalize(workspace_key)
        for path, name in self.paths:
            if key == path or key.startswith(path.rstrip("/") + "/"):
                return name
        return UNMAPPED


def normalize(path: str) -> str:
    stripped = path.rstrip("/")
    return stripped or "/"


def merge(records: Iterable[Record]) -> dict[str, Job]:
    jobs: dict[str, Job] = {}
    for record in records:
        jobs.setdefault(record.job_id, Job(record.job_id)).add(record)
    return jobs


def day_of(ended_ms: int | None) -> str:
    if ended_ms is None:
        return UNKNOWN_DAY
    moment = dt.datetime.fromtimestamp(ended_ms / 1000, tz=dt.timezone.utc)
    return moment.date().isoformat()


def rate(numerator: int, denominator: int) -> float | None:
    return numerator / denominator if denominator else None


def summarize(jobs: list[Job]) -> dict[str, object]:
    with_totals = [job.totals for job in jobs if job.totals is not None]
    # With no totals at all the sums are unknown, not zero.
    sums: dict[str, int | None] = {
        name: sum(t[name] for t in with_totals) if with_totals else None
        for name in TOTAL_FIELDS
    }
    # A job with totals but no lookups has no hit rate of its own.
    per_job = [
        r
        for r in (rate(t["hits"], t["hits"] + t["misses"]) for t in with_totals)
        if r is not None
    ]
    return {
        "jobs": len(jobs),
        "jobs_with_totals": len(with_totals),
        "jobs_without_totals": len(jobs) - len(with_totals),
        "hits": sums["hits"],
        "misses": sums["misses"],
        "median_job_hit_rate": statistics.median(per_job) if per_job else None,
        "aggregate_hit_rate": (
            rate(sums["hits"], sums["hits"] + sums["misses"]) if with_totals else None
        ),
        "puts_accepted": sums["puts_accepted"],
        "puts_duplicate": sums["puts_duplicate"],
        "puts_failed": sums["puts_failed"],
        "puts_refused": sums["puts_refused"],
        "put_bytes_accepted": sums["put_bytes_accepted"],
    }


def build_report(
    records: Iterable[Record],
    workspaces: Workspaces,
    repositories: Repositories,
    cutover: dt.date | None = None,
) -> dict[str, object]:
    by_day: dict[tuple[str, str], list[Job]] = {}
    by_period: dict[tuple[str, str], list[Job]] = {}
    for job in merge(records).values():
        ended_ms = job.ended_ms
        if ended_ms is None:
            ended_ms = workspaces.terminal_ms.get(job.job_id)
        key = workspaces.keys.get(job.job_id, job.fallback_workspace_key)
        repository = repositories.name(key)
        day = day_of(ended_ms)
        by_day.setdefault((repository, day), []).append(job)
        if cutover is not None and day != UNKNOWN_DAY:
            period = "after" if dt.date.fromisoformat(day) >= cutover else "before"
            by_period.setdefault((repository, period), []).append(job)
    report: dict[str, object] = {
        "basis": BASIS,
        "rows": [
            {"repository": repository, "day": day, **summarize(jobs)}
            for (repository, day), jobs in sorted(by_day.items(), key=row_order)
        ],
    }
    if cutover is not None:
        report["cutover"] = cutover.isoformat()
        report["comparison"] = [
            {"repository": repository, "period": period, **summarize(jobs)}
            for (repository, period), jobs in sorted(
                by_period.items(),
                key=lambda item: (item[0][0] == UNMAPPED, item[0][0], item[0][1] != "before"),
            )
        ]
    return report


def row_order(item: tuple[tuple[str, str], list[Job]]) -> tuple[bool, str, bool, str]:
    (repository, day), _ = item
    return (repository == UNMAPPED, repository, day == UNKNOWN_DAY, day)


def cell(value: object) -> str:
    if value is None:
        return ""
    if isinstance(value, float):
        return f"{value:.4f}"
    return str(value)


def table(rows: list[dict[str, object]], label: str) -> list[str]:
    header = ("repository", label, *COLUMNS)
    return ["\t".join(header)] + [
        "\t".join(cell(row[name]) for name in header) for row in rows
    ]


def render_text(report: dict[str, object]) -> str:
    lines = ["Remote compile cache per repository and UTC day the job ended.", BASIS, ""]
    lines += table(report["rows"], "day")
    if "comparison" in report:
        lines += [
            "",
            f"Before and after {report['cutover']} (UTC days on or after it are after),"
            " on the same combined sccache and ccache basis.",
            "",
        ]
        lines += table(report["comparison"], "period")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--host-log", action="append", default=[], type=Path,
                        help="motor-host journal export (short-iso text or json)")
    parser.add_argument("--runner-jobs", action="append", default=[], type=Path,
                        help="runner record.json, job directory, jobs directory or state directory")
    parser.add_argument("--ck-motor", action="append", default=[], type=Path,
                        help="ck-motor storage directory, state.json or journal segment")
    parser.add_argument("--repositories", required=True, type=Path,
                        help="JSON object mapping local checkout paths to repository names")
    parser.add_argument("--cutover", type=dt.date.fromisoformat,
                        help="UTC day (YYYY-MM-DD) from which jobs count as after")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    args = parser.parse_args(argv)
    if not args.host_log and not args.runner_jobs:
        parser.error("give at least one --host-log or --runner-jobs source")

    try:
        repositories = Repositories(json.loads(args.repositories.read_text()))
    except (OSError, ValueError) as error:
        parser.error(f"--repositories: {error}")
    workspaces = Workspaces()
    for path in args.ck_motor:
        workspaces.add_path(path)
    records: list[Record] = []
    for path in args.host_log:
        with path.open(encoding="utf-8", errors="replace") as lines:
            records.extend(read_host_log(lines))
    for path in args.runner_jobs:
        records.extend(read_runner_jobs(path))

    report = build_report(records, workspaces, repositories, args.cutover)
    if args.format == "json":
        json.dump(report, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        sys.stdout.write(render_text(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
