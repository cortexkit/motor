#!/usr/bin/env python3
"""Reject a restart unless all unfinished job records can close or reattach
without rerunning commands. Started Windows jobs are unsafe even with a recorded
unit; other jobs with recorded units, proven-not-started jobs, and processes
that have already exited are safe. Uncertain records are not.
"""
import json
import os
from pathlib import Path
import sys


def recoverable(job):
    # A record without a request (null) must classify, not crash the inventory.
    if ((job.get("request") or {}).get("platform") == "windows"
            and job.get("started_ms") is not None):
        return False
    process = job.get("process")
    if process and process.get("unit"):
        return True
    if (job.get("state") in ("AwaitingBundles", "Queued")
            and job.get("started_ms") is None and process is None):
        return True
    if process and process.get("pid", 0) > 0:
        try:
            os.kill(process["pid"], 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            pass
    return False


def main(root):
    jobs = Path(root) / "jobs"
    if not jobs.is_dir():
        raise ValueError(f"cannot inventory survivors: {jobs} is missing")
    unsafe = []
    for directory in sorted(jobs.iterdir()):
        if not directory.is_dir():
            continue
        job = json.loads((directory / "record.json").read_text())
        if job.get("terminal") is not None:
            continue
        safe = recoverable(job)
        print(f"survivor {job['id']}: {'restart-recoverable' if safe else 'UNSAFE'} state={job.get('state')}", file=sys.stderr)
        if not safe:
            unsafe.append(job["id"])
    return 1 if unsafe else 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1]))
    except (OSError, ValueError, KeyError, IndexError) as error:
        print(f"survivor inventory failed: {error}", file=sys.stderr)
        sys.exit(1)
