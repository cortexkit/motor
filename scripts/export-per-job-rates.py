#!/usr/bin/env python3
"""Export a rate evidence bundle from motor-host journal JSONL and broker usage.

Journal files must be raw `journalctl -u motor-host -o json` output, including
records bracketing the window. Usage files are JSON arrays of usage bodies
exported from the broker's accepted reports, NOT motor-host's pending outbox.
Config JSON supplies window {start_ms, end_ms}, tenant_host, per_job_host,
canary_jtis and tolerances {lost, host_stop, hosted_host_full} as rate fractions.

--max-gap-ms is required and has no default. Coverage means no silence longer
than max_gap_ms, not proof that no record was lost. A longer silence, boot ID
change or journal cursor sequence-ID change ends an interval. Nothing before
the first record or after the last counts. Sparse logs may need a separate
periodic host coverage marker before a useful bound can be chosen.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import shlex
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location("per_job_rates", Path(__file__).with_name("per-job-rates.py"))
if spec is None or spec.loader is None:
    raise ImportError("cannot load per-job-rates.py")
rates = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rates)


def read_journal(path: Path) -> list[dict]:
    rows = []
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if line.strip():
            try:
                row = json.loads(line)
                if not isinstance(row, dict):
                    raise ValueError("expected a journal object")
                rows.append(row)
            except ValueError as error:
                raise ValueError(f"{path}:{number}: {error}") from error
    return rows


def journal_export(rows: list[dict], max_gap_ms: int) -> dict:
    rates.integer(max_gap_ms, "max_gap_ms", 1)
    intervals, claims, stops = [], [], []
    previous = None
    hostname = None
    for row in rows:
        raw_time = rates.text(row["__REALTIME_TIMESTAMP"], "journal timestamp")
        if not raw_time.isdecimal():
            raise ValueError("journal timestamp must be microseconds since the epoch")
        timestamp = int(raw_time)
        boot = rates.text(row["_BOOT_ID"], "journal _BOOT_ID")
        cursor = rates.text(row["__CURSOR"], "journal __CURSOR")
        sequence = re.search(r"(?:^|;)s=([^;]+)(?:;|$)", cursor)
        if not sequence:
            raise ValueError("journal cursor has no sequence ID (s=)")
        sequence = sequence[1]
        source = rates.text(row["_HOSTNAME"], "journal _HOSTNAME")
        if hostname is not None and source != hostname:
            raise ValueError("journal combines multiple hosts")
        hostname = source
        if row.get("_SYSTEMD_UNIT") != "motor-host.service":
            raise ValueError("journal must contain only motor-host.service records")
        if previous and timestamp < previous[0]:
            raise ValueError("journal time goes backwards; cannot establish coverage")
        if (previous is None or timestamp - previous[0] > max_gap_ms * 1000
                or boot != previous[1] or sequence != previous[2]):
            intervals.append({
                "start_ms": (timestamp + 999) // 1000,
                "end_ms": timestamp // 1000,
                "boot_id": boot,
                "sequence_id": sequence,
                "first_cursor": cursor,
                "last_cursor": cursor,
            })
        current = intervals[-1]
        current["end_ms"] = timestamp // 1000
        current["last_cursor"] = cursor
        previous = (timestamp, boot, sequence)

        # Match only the event name in the logger's message, never a field value.
        message = rates.text(row["MESSAGE"], "journal MESSAGE")
        event = re.match(
            r"^\[motor-host\] (?:TRACE|DEBUG|INFO|WARN|ERROR) [^ ]+: "
            r"(run_claim|job_host_stop)(?: |$)", message,
        )
        if not event:
            continue
        fields = {}
        for item in shlex.split(message[event.end():]):
            if "=" not in item:
                raise ValueError(f"malformed {event[1]} log fields")
            key, value = item.split("=", 1)
            if key in fields:
                raise ValueError(f"duplicate {key} in {event[1]}")
            fields[key] = value
        common = {"at_ms": timestamp // 1000, "jti": rates.text(fields["jti"], "log jti")}
        if event[1] == "run_claim":
            claim = {**common, "mode": fields["mode"], "result": fields["result"]}
            if fields["result"] == "refused":
                claim.update(reason=fields["reason"], cause=fields["cause"])
            claims.append(claim)
        else:
            stops.append({**common, "cause": fields["cause"]})
    # A lone sub-millisecond entry covers no complete millisecond.
    intervals = [item for item in intervals if item["start_ms"] <= item["end_ms"]]
    return {"coverage": intervals, "claims": claims, "host_stops": stops}


def export_bundle(config: dict, journals: dict, usages: dict, max_gap_ms: int) -> dict:
    rates.integer(max_gap_ms, "max_gap_ms", 1)
    hosts = []
    for mode in ("tenant", "per_job"):
        usage = usages[mode]
        if not isinstance(usage, list) or any(not isinstance(body, dict) for body in usage):
            raise ValueError(f"{mode}: broker accepted usage must be an array of bodies")
        hosts.append({
            "host": config[f"{mode}_host"],
            "mode": mode,
            **journal_export(journals[mode], max_gap_ms),
            "accepted_usage": usage,
        })
    return {
        "window": config["window"],
        "tolerances": config["tolerances"],
        "canary_jtis": config["canary_jtis"],
        "coverage_policy": {"max_gap_ms": max_gap_ms, "limitation": rates.COVERAGE_LIMITATION},
        "hosts": hosts,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--max-gap-ms", required=True, type=int)
    for mode in ("tenant", "per-job"):
        parser.add_argument(f"--{mode}-journal", required=True, type=Path)
        parser.add_argument(f"--{mode}-usage", required=True, type=Path)
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text())
        journals = {mode: read_journal(getattr(args, f"{mode}_journal")) for mode in ("tenant", "per_job")}
        usages = {mode: json.loads(getattr(args, f"{mode}_usage").read_text()) for mode in ("tenant", "per_job")}
        bundle = export_bundle(config, journals, usages, args.max_gap_ms)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"export-per-job-rates: {error}", file=sys.stderr)
        return 2
    print(json.dumps(bundle, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
