#!/usr/bin/env python3
"""Decode credential-free CLI records and verify independent guest evidence."""
import argparse
import base64
import hashlib
import json
from pathlib import Path


def decode(path):
    accepted = terminal = started = None
    outputs = {}
    for line in Path(path).read_text().splitlines():
        record = json.loads(line)
        kind = record["type"]
        if kind == "accepted":
            if record.get("platform") != "macos":
                raise ValueError("Accepted.platform is not macos")
            if accepted and record["job_id"] != accepted["job_id"]:
                raise ValueError("Accepted job ID changed")
            accepted = record
        elif kind == "started":
            if record["seq"] != 0:
                raise ValueError("Started sequence is not zero")
            started = record
        elif kind == "output":
            seq = record["seq"]
            if seq in outputs and outputs[seq] != record:
                raise ValueError("replayed output differs")
            if seq not in outputs and seq != len(outputs) + 1:
                raise ValueError("lost or reordered output sequence")
            if record["stream"] not in ("stdout", "stderr"):
                raise ValueError("unexpected output stream")
            base64.b64decode(record["bytes"], validate=True)
            outputs[seq] = record
        elif kind == "terminal":
            terminal = record
        else:
            raise ValueError("unexpected capture record: " + kind)
    if not accepted or not started or not terminal:
        raise ValueError("missing Accepted, Started or Terminal")
    if terminal["job_id"] != accepted["job_id"]:
        raise ValueError("Terminal job ID differs")
    streams = {name: b"".join(base64.b64decode(record["bytes"], validate=True)
               for record in outputs.values() if record["stream"] == name)
               for name in ("stdout", "stderr")}
    return accepted, terminal, streams


def verify(directory):
    directory = Path(directory)
    accepted, terminal, streams = decode(directory / "records.jsonl")
    guest = json.loads((directory / "guest.status.json").read_text())
    code = guest["returncode"]
    outcome = terminal["outcome"]
    # The capture wrapper exits with shell-style 128+signal when its child is
    # signalled; the runner observes the wrapper's exit, not that child's signal.
    exit_code = code if code >= 0 else 128 - code
    expected = {"type": "exit", "code": exit_code}
    if outcome != expected:
        raise ValueError(f"guest status {expected} differs from Terminal {outcome}")
    cli = int((directory / "cli.status").read_text())
    if cli != exit_code:
        raise ValueError("CLI exit differs from independent guest status")
    result = {"job_id": accepted["job_id"], "platform": accepted["platform"], "guest": guest, "terminal": terminal}
    for name, data in streams.items():
        (directory / ("output." + name)).write_bytes(data)
        if data != (directory / ("guest." + name)).read_bytes():
            raise ValueError("guest/Output byte mismatch: " + name)
        result[name] = {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    (directory / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.directory), indent=2))
