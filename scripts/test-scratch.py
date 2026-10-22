#!/usr/bin/env python3
"""Refuse global temporary-directory constructors in Rust tests.

An allowlist can preserve production OS-root reads unrelated to test scratch;
it limits each exception by file, enclosing function and existing call count.
Tests use cortexkit_test_support::scratch_root(), the published fenced helper;
its OS-root derivation lives outside this repository and needs no exemption.
Untracked Rust files are checked too, so adding a suite cannot bypass the gate.
"""
import sys

if sys.version_info < (3, 11):
    sys.exit("Python 3.11 or newer is required; run: python3.11 scripts/test-scratch.py")

from pathlib import Path
import re
import runpy

ROOT = Path(__file__).resolve().parent.parent
# The motor census reads 58 non-symlink crates/**/*.rs files with the loop below.
# Retain at least 50 (about 85%) to permit modest shrinkage, not an empty scan.
MINIMUM_FILES = 50
structural_source = runpy.run_path(str(ROOT / "scripts/unreachable-modules.py"))["structural_source"]
CALL = re.compile(r"(?:\b(?:std\s*::\s*)?env\s*::\s*temp_dir|\btempfile\s*::\s*tempdir|\bTempDir\s*::\s*new|\.\s*tempdir|\btemp_dir|\btempdir)\s*\(\s*\)")
FUNCTION = re.compile(r"\bfn\s+(\w+)\s*[(<]")
TEMP_ENV = re.compile(r"\b(?:(?:std\s*::\s*)?env\s*::\s*)?var(?:_os)?\s*\([^)]*\)")


def functions(code):
    spans = []
    for match in FUNCTION.finditer(code):
        start = code.find("{", match.end())
        if start < 0:
            continue
        depth = 1
        end = start + 1
        while end < len(code) and depth:
            depth += (code[end] == "{") - (code[end] == "}")
            end += 1
        spans.append((start, end, match.group(1)))
    return spans


def file_violations(source, relative, allow):
    code = structural_source(source)
    matches = list(CALL.finditer(code))
    matches += [match for match in TEMP_ENV.finditer(code)
                if re.search(r'"(?:TMPDIR|TMP|TEMP)"', source[match.start():match.end()])]
    spans = functions(code)
    seen = {}
    errors = []
    for match in matches:
        # Masked string arguments are not zero-argument calls to the OS API:
        # several suites have their own temp_dir("suite-name") helper.
        if match.group().startswith(("temp_dir", "tempdir")) and not re.fullmatch(r"(?:temp_dir|tempdir)\s*\(\s*\)", source[match.start():match.end()]):
            continue
        enclosing = [s for s in spans if s[0] <= match.start() < s[1]]
        symbol = min(enclosing, key=lambda s: s[1] - s[0])[2] if enclosing else "<module>"
        key = relative, symbol
        seen[key] = seen.get(key, 0) + 1
        # Exceptions permit only the existing OS-root reads, never a raw
        # TempDir constructor added inside an otherwise allowed function.
        os_root_read = bool(re.fullmatch(r"std\s*::\s*env\s*::\s*temp_dir\s*\(\s*\)", match.group()))
        if not os_root_read or seen[key] > allow.get(key, 0):
            line = code.count("\n", 0, match.start()) + 1
            errors.append(f"{relative}:{line}: {symbol}: use cortexkit_test_support::scratch_root(), not {match.group().strip()}")
    return errors


def self_check():
    refused = [
        "std::env::temp_dir()", "env::temp_dir()", "tempfile::tempdir()",
        "TempDir::new()", "builder.tempdir()", "temp_dir()", "tempdir()",
        'std::env::var("TMPDIR")', 'std::env::var_os("TMP")',
        'var("TEMP")',
    ]
    for form in refused:
        source = f"fn planted() {{ let _ = {form}; }}"
        if not file_violations(source, "planted.rs", {}):
            return [f"self-check missed refused form: {form}"]
    if file_violations("fn allowed() { let _ = scratch_root(); }", "planted.rs", {}):
        return ["self-check rejected allowed form: scratch_root()"]
    return []


def violations(root=ROOT, minimum_files=MINIMUM_FILES):
    allow = {}
    for line in (root / "scripts/test-scratch.allow").read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            path, symbol, count = line.split()
            allow[path, symbol] = int(count)
    errors = []
    visited = 0
    for path in sorted((root / "crates").rglob("*.rs")):
        if path.is_symlink():
            continue
        source = path.read_text()
        visited += 1
        errors.extend(file_violations(source, path.relative_to(root).as_posix(), allow))
    if visited < minimum_files:
        errors.append(f"tree scan read {visited} Rust files; expected at least {minimum_files}")
    return errors


def main():
    errors = self_check()
    if not errors:
        errors = violations()
    if errors:
        print("test scratch root gate failed:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("test scratch root: all test constructors use cortexkit_test_support::scratch_root (11 self-check cases)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
