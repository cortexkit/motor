#!/usr/bin/env python3
"""Check wasm-opt 120 by running a real WebAssembly module through two passes."""

import hashlib
import json
import re
import subprocess
import tempfile
from pathlib import Path


EXPECTED_MAJOR = "120"
TINY_MODULE = bytes.fromhex("0061736d01000000010401600000030201000a040102000b")


def command(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True, timeout=30)


def probe():
    version = command("wasm-opt", "--version").stdout.strip()
    actual_major = re.match(r"wasm-opt version (\d+)(?:\s|$)", version)
    if not actual_major or actual_major.group(1) != EXPECTED_MAJOR:
        raise ValueError(f"expected wasm-opt major {EXPECTED_MAJOR}, observed {version!r}")
    with tempfile.TemporaryDirectory(prefix="wasm-opt-probe-") as directory:
        source = Path(directory) / "input.wasm"
        optimized = Path(directory) / "optimized.wasm"
        round_trip = Path(directory) / "round-trip.wasm"
        source.write_bytes(TINY_MODULE)
        command("wasm-opt", str(source), "-o", str(optimized))
        command("wasm-opt", str(optimized), "-o", str(round_trip))
        result = round_trip.read_bytes()
        if not result.startswith(b"\0asm\1\0\0\0"):
            raise ValueError("wasm-opt did not produce a valid WebAssembly module header")
        return {"passed": True, "version": version,
                "module_sha256": hashlib.sha256(TINY_MODULE).hexdigest(),
                "round_trip_sha256": hashlib.sha256(result).hexdigest(),
                "round_trip_bytes": len(result)}


def main():
    try:
        report = probe()
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        report = {"passed": False, "error": str(error)}
    print(json.dumps(report, sort_keys=True))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
