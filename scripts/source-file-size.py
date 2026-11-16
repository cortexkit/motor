#!/usr/bin/env python3
"""Keep hand-written source files small enough to edit and rebuild quickly.

A file over the limit is usually several areas in one place, and every edit to
it recompiles all of them. Tests and generated files are exempt. A file that was
already over the limit when the check arrived is listed in EXCEPTIONS with its
size then and the reason; it may shrink but never grow, and it is split by area
the next time it is worked on.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

LIMIT = 5000
SOURCE_SUFFIXES = (".rs", ".py", ".sh", ".ps1", ".swift")

# path: (most lines allowed, why it is over the limit)
EXCEPTIONS = {
    "crates/motor-runner/src/engine.rs": (
        12942,
        "the runner's job engine predates the limit; split it by area (admission, "
        "dispatch, lifecycle, retention) when next working in it",
    ),
}


def exempt(path: str) -> bool:
    parts = path.split("/")
    name = parts[-1]
    return (
        "tests" in parts[:-1]
        or "test-vectors" in parts
        or name in ("tests.rs", "test.rs")
        or name.endswith(("_tests.rs", "_test.rs"))
        or (name.startswith("test_") and name.endswith(".py"))
        or "generated" in name
    )


def violations(root: Path) -> list[str]:
    listed = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"], check=True, capture_output=True
    ).stdout.decode().split("\0")
    found = []
    for path in sorted(p for p in listed if p.endswith(SOURCE_SUFFIXES)):
        if exempt(path):
            continue
        file = root / path
        if not file.is_file():
            continue
        lines = sum(1 for _ in file.open("rb"))
        allowed, _reason = EXCEPTIONS.get(path, (LIMIT, ""))
        if lines > allowed:
            if path in EXCEPTIONS:
                found.append(f"{path}: {lines} lines; it may not grow past {allowed}, split it by area")
            else:
                found.append(f"{path}: {lines} lines, over the {LIMIT}-line limit; split it by area")
    for path in sorted(EXCEPTIONS):
        if not (root / path).is_file():
            found.append(f"{path}: listed in EXCEPTIONS but gone; remove the entry")
    return found


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    found = violations(root)
    if found:
        print("Hand-written source files must stay small enough to rebuild quickly:", file=sys.stderr)
        for entry in found:
            print(f"  {entry}", file=sys.stderr)
        return 1
    print(f"source-file-size: OK (limit {LIMIT} lines, {len(EXCEPTIONS)} named exception)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
