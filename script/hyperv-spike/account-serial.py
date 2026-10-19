#!/usr/bin/env python3
"""Compare completed host/guest serial I/O; counts do not locate a physical loss hop."""
import argparse
import json
from pathlib import Path
import re

PREFIX = "EVIDENCE transport "
DONE = re.compile(r"\bio (read|write) done bytes=(\d+) total=(\d+)\b")
STATUS = re.compile(r"COM status errors=0x([0-9a-fA-F]+).*queued_in=(\d+)")


def text(path):
    data = Path(path).read_bytes()
    return data.decode("utf-16" if data[:2] in (b"\xff\xfe", b"\xfe\xff") else "utf-8-sig")


def advance(totals, direction, count, reported):
    if type(count) is not int or type(reported) is not int or count < 0:
        raise ValueError("invalid byte counter")
    expected = totals[direction] + count
    if reported != expected:
        raise ValueError(f"inconsistent {direction} counter: expected {expected}, got {reported}; use one client stderr log, not duplicated host transcript")
    totals[direction] = expected


def account(host, guest):
    host_totals = {"read": 0, "write": 0}
    guest_totals = {"read": 0, "write": 0}
    host_seen = guest_seen = 0
    for line in host.splitlines():
        if not line.startswith(PREFIX):
            continue
        event = json.loads(line[len(PREFIX):])
        if event["event"] not in ("read-done", "write-done"):
            continue
        direction = event["event"].split("-")[0]
        detail = event["detail"]
        advance(host_totals, direction, detail["bytes"], detail["total"])
        host_seen += 1
    flags = 0
    peak_queue = 0
    last_frame = None
    for line in guest.splitlines():
        match = DONE.search(line)
        if match:
            direction, count, reported = match.groups()
            advance(guest_totals, direction, int(count), int(reported))
            guest_seen += 1
        match = STATUS.search(line)
        if match:
            flags |= int(match[1], 16)
            peak_queue = max(peak_queue, int(match[2]))
        if "reader frame " in line:
            last_frame = line.split("reader frame ", 1)[1]
    if not host_seen or not guest_seen:
        raise ValueError("both logs need explicit completed-I/O byte counters")
    if guest_totals["read"] > host_totals["write"]:
        raise ValueError("guest completed more RX than this host trace sent")
    shortfall = host_totals["write"] - guest_totals["read"]
    return {
        "host_accepted_tx": host_totals["write"],
        "guest_completed_rx": guest_totals["read"],
        "end_to_end_shortfall": shortfall,
        "guest_completed_tx": guest_totals["write"],
        "host_completed_rx": host_totals["read"],
        "reverse_counts_match": guest_totals["write"] == host_totals["read"],
        "guest_error_flags_history": f"0x{flags:x}",
        "guest_peak_input_queue": peak_queue,
        "last_parsed_host_frame": last_frame,
        "classification": "receive-stream shortfall" if shortfall else "completed counts match; content and later stages still need validation",
        "scope": "accepted named-pipe writes versus completed guest reads; no exact internal loss hop inferred",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host-trace", required=True, type=Path, help="one run-N.stderr.log, not host.log")
    parser.add_argument("--guest-log", required=True, type=Path)
    args = parser.parse_args()
    try:
        result = account(text(args.host_trace), text(args.guest_log))
    except (OSError, ValueError, KeyError) as error:
        raise SystemExit(f"FAIL serial-accounting {error}") from error
    print(json.dumps(result, indent=2))
    if result["end_to_end_shortfall"] or not result["reverse_counts_match"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
