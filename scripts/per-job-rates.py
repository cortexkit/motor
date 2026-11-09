#!/usr/bin/env python3
"""Compare per-job host rates with tenant mode using an exported evidence bundle.

Run export-per-job-rates.py first. Rates use distinct authenticated Run identifiers
(jti values) claimed in [start_ms, end_ms), not just started jobs. Accepted usage and host
stops for that cohort may settle after the window. Canary jtis are excluded.
Tolerances are owner-set absolute rate differences (0.01 = one percentage point).
Coverage means no silence longer than max_gap_ms, not proof no record was lost.
Exit codes: 0 within tolerance, 1 exceeded tolerance, 2 invalid/incomplete evidence.
"""

from __future__ import annotations

import argparse
import json
import sys
from fractions import Fraction
from pathlib import Path

METRICS = ("lost", "host_stop", "hosted_host_full")
HOST_CAUSES = {"host_oom", "host_pressure"}
COVERAGE_LIMITATION = (
    "Coverage means no silence longer than max_gap_ms, not proof that no record was lost."
)


def integer(value: object, label: str, minimum: int = 0) -> int:
    if type(value) is not int or value < minimum:
        raise ValueError(f"{label} must be an integer >= {minimum}")
    return value


def text(value: object, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{label} must be a nonempty string")
    return value


def uncovered(intervals: list, start: int, end: int) -> list[list[int]]:
    cursor = start
    gaps = []
    for interval in sorted(intervals, key=lambda item: item["start_ms"]):
        left = integer(interval["start_ms"], "coverage start_ms")
        right = integer(interval["end_ms"], "coverage end_ms")
        if left > right:
            raise ValueError("coverage interval ends before it starts")
        if left > cursor:
            gaps.append([cursor, min(left, end)])
        cursor = max(cursor, right)
        if cursor >= end:
            break
    if cursor < end:
        gaps.append([cursor, end])
    return gaps


def host_rates(host: dict, start: int, end: int, canaries: set[str]) -> dict:
    name = text(host["host"], "host")
    gaps = uncovered(host["coverage"], start, end)
    if gaps:
        raise ValueError(f"{name}: log does not cover the whole window; uncovered ms: {gaps}")

    cohort, refused, quarantined = set(), set(), set()
    for claim in host["claims"]:
        at = integer(claim["at_ms"], "claim at_ms")
        job = text(claim["jti"], "claim jti")
        if not start <= at < end or job in canaries:
            continue
        if claim["mode"] != host["mode"]:
            raise ValueError(f"{name}: claim mode differs from host mode")
        result = claim["result"]
        if result not in {"admitted", "waiting", "refused"}:
            raise ValueError(f"{name}: unknown claim result {result!r}")
        cohort.add(job)
        if result == "refused":
            reason = text(claim["reason"], "refusal reason")
            cause = text(claim["cause"], "refusal cause")
            # Use the client-facing hosted_host_full name for logged host_full refusals.
            if reason == "host_full":
                (quarantined if cause == "quarantined_workspace" else refused).add(job)

    if not cohort:
        raise ValueError(f"{name}: no non-canary authenticated Run jtis in the window")

    lost, stopped, usage_by_job = set(), set(), {}
    for usage in host["accepted_usage"]:
        job = text(usage["job_id"], "usage job_id")
        if job not in cohort:
            continue
        if job in usage_by_job and usage_by_job[job] != usage:
            raise ValueError(f"{name}: conflicting broker-accepted usage for {job}")
        usage_by_job[job] = usage
        if usage["outcome"] not in {"exit", "signal", "timeout", "cancelled", "lost"}:
            raise ValueError(f"{name}: unknown usage outcome")
        if "oom_kill" in usage and usage["oom_kill"] not in HOST_CAUSES | {"job_limit"}:
            raise ValueError(f"{name}: unknown usage oom_kill")
        if usage["outcome"] == "lost":
            lost.add(job)
        if usage.get("oom_kill") in HOST_CAUSES:
            stopped.add(job)
    for stop in host["host_stops"]:
        if stop["cause"] not in HOST_CAUSES:
            raise ValueError(f"{name}: unknown host stop cause")
        if stop["jti"] in cohort:
            stopped.add(stop["jti"])

    numerators = {
        "lost": lost,
        "host_stop": stopped,
        "hosted_host_full": refused,
        "quarantined_workspace": quarantined,
    }
    return {
        "host": name,
        "mode": host["mode"],
        "denominator": len(cohort),
        "accepted_usage_jtis": len(usage_by_job),
        "rates": {
            metric: {"count": len(jobs), "rate": len(jobs) / len(cohort)}
            for metric, jobs in numerators.items()
        },
    }


def compare(bundle: dict) -> dict:
    start = integer(bundle["window"]["start_ms"], "window start_ms")
    end = integer(bundle["window"]["end_ms"], "window end_ms")
    if start >= end:
        raise ValueError("window end_ms must be after start_ms")
    max_gap = integer(bundle["coverage_policy"]["max_gap_ms"], "max_gap_ms", 1)
    canaries = {text(job, "canary jti") for job in bundle["canary_jtis"]}
    hosts = bundle["hosts"]
    if len(hosts) != 2 or {host["mode"] for host in hosts} != {"tenant", "per_job"}:
        raise ValueError("exactly one tenant and one per_job host are required")
    if hosts[0]["host"] == hosts[1]["host"]:
        raise ValueError("tenant and per_job host names must differ")
    rates = {host["mode"]: host_rates(host, start, end, canaries) for host in hosts}
    comparisons = {}
    for metric in METRICS:
        tolerance = bundle["tolerances"][metric]
        if type(tolerance) not in (int, float) or not 0 <= tolerance <= 1:
            raise ValueError(f"{metric}: tolerance must be a rate fraction in [0, 1]")
        tenant, per_job = rates["tenant"], rates["per_job"]
        baseline = Fraction(tenant["rates"][metric]["count"], tenant["denominator"])
        candidate = Fraction(per_job["rates"][metric]["count"], per_job["denominator"])
        comparisons[metric] = {
            "tenant_rate": float(baseline),
            "per_job_rate": float(candidate),
            "difference": float(candidate - baseline),
            "tolerance": tolerance,
            "passed": candidate - baseline <= Fraction(str(tolerance)),
        }
    return {
        "window": bundle["window"],
        "coverage_policy": {"max_gap_ms": max_gap, "limitation": COVERAGE_LIMITATION},
        "hosts": rates,
        "comparisons": comparisons,
        "passed": all(item["passed"] for item in comparisons.values()),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path, help="JSON output from export-per-job-rates.py")
    args = parser.parse_args()
    bundle = {}
    try:
        parsed = json.loads(args.bundle.read_text())
        if not isinstance(parsed, dict):
            raise ValueError("bundle must be a JSON object")
        bundle = parsed
        report = compare(bundle)
    except (OSError, ValueError, KeyError, TypeError) as error:
        policy = bundle.get("coverage_policy", {})
        if not isinstance(policy, dict):
            policy = {}
        print(f"per-job-rates: {error}; max_gap_ms={policy.get('max_gap_ms', 'missing')}. "
              f"{COVERAGE_LIMITATION}", file=sys.stderr)
        return 2
    print(json.dumps(report, indent=2, allow_nan=False))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
