#!/usr/bin/env python3
"""Keep integration suites folded while preserving documented process isolation."""
from pathlib import Path
import json
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
ALLOWLIST = ROOT / "scripts/test-binary-fold.allow"
REMEDY = ("move the file into tests/it/ as a module, or add it to the allowlist "
          "with the reason it needs its own process")


def read_allowlist(path=ALLOWLIST):
    allowed = set()
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        entry, separator, reason = line.partition("\t")
        if not separator or not reason.strip() or entry in allowed:
            raise ValueError(f"{path}:{number}: expected a unique path and a nonempty reason separated by a tab")
        allowed.add(entry)
    return allowed


def cargo_test_entrypoints(root):
    output = subprocess.check_output(
        ["env", "-u", "GIT_CONFIG_COUNT", "-u", "GIT_CONFIG_KEY_0", "-u", "GIT_CONFIG_VALUE_0",
         "cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"], cwd=root
    )
    return [
        Path(target["src_path"])
        for package in json.loads(output)["packages"]
        for target in package["targets"]
        if "test" in target["kind"]
    ]


def violations(root=ROOT, allowed=None, cargo_entries=()):
    if allowed is None:
        allowed = read_allowlist()
    errors = []
    for crate in sorted((root / "crates").iterdir()):
        tests = crate / "tests"
        if not tests.is_dir():
            continue
        entries = set(tests.glob("*.rs"))
        entries.update(tests.glob("*/main.rs"))
        # Cargo can name an arbitrary file as an explicit directory target;
        # include those entry points as well as the auto-discovered layout.
        entries.update(entry for entry in cargo_entries if tests in entry.parents)
        for entry in sorted(entries):
            relative = entry.relative_to(root).as_posix()
            if relative not in allowed:
                errors.append(f"{relative}: {REMEDY}")
    return errors


def main():
    try:
        errors = violations(cargo_entries=cargo_test_entrypoints(ROOT))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"test binary fold gate failed: {error}", file=sys.stderr)
        return 1
    if errors:
        print("test binary fold gate failed:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("test binary fold: every integration entry point is allowlisted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
