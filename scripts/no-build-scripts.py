#!/usr/bin/env python3
"""Keep workspace crates shareable across checkout paths and commits."""

from __future__ import annotations

import sys
from pathlib import Path

if sys.version_info < (3, 11):
    print("Python 3.11 or newer is required; run: python3.11 scripts/no-build-scripts.py", file=sys.stderr)
    raise SystemExit(1)

import tomllib


def has_build_key(value: object) -> bool:
    if isinstance(value, dict):
        return "build" in value or any(has_build_key(child) for child in value.values())
    if isinstance(value, list):
        return any(has_build_key(child) for child in value)
    return False


def violations(root: Path) -> list[str]:
    crates = root / "crates"
    found = [str(path.relative_to(root)) for path in crates.rglob("build.rs")]
    for manifest in sorted(crates.glob("*/Cargo.toml")):
        if has_build_key(tomllib.loads(manifest.read_text())):
            found.append(f"{manifest.relative_to(root)}: build key")
    return sorted(found)


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    found = violations(root)
    if found:
        print("Build scripts are forbidden: crates with build scripts rebuild per checkout path and per commit.", file=sys.stderr)
        for entry in found:
            print(f"  {entry}", file=sys.stderr)
        return 1
    print("no-build-scripts: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
