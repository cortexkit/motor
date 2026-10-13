#!/usr/bin/env python3
"""Refuse Rust sources that locate or spawn the subc daemon outside the helper.

A spawned ck-subc resolves its own paths (run dir, logs, connection file,
config, cache) from the environment. A test that isolates only the connection
file still lets the daemon write into the operator's production data home
(~/.local/share/cortexkit/run/logs), which `ck module logs` and the doctors
read. Every test daemon must therefore come from
`cortexkit_test_support::SubcDaemonBinary`: it is the only code that builds
the binary, it never exposes the binary's path, and its `command()` applies
`fenced_subc_daemon_env`, which redirects every ambient root into the test's
scratch directory.

The published helper lives outside motor. Two rules hold for every Rust file
under motor's crates/; no local helper source is exempt:

1. No string literal names the daemon binary as a program: `"ck-subc"`, a
   path ending in `/ck-subc`, or either with `.exe`. That is how a rig builds
   (`cargo build --bin ck-subc`) or locates (`debug/ck-subc`) the binary, so
   refusing it means a rig cannot hold a raw path to spawn. Prose that merely
   mentions the daemon ("spawn ck-subc", comments) does not match.
2. No `Command::new(<ident>)` whose program identifier contains "subc"
   (subc_core, ck_subc, ...). With rule 1 this cannot compile against a
   `SubcDaemonBinary`, but a path smuggled in some other way would still be
   refused here by name.

Untracked Rust files are checked too, so adding a suite cannot bypass the gate.
"""
from pathlib import Path
import re
import runpy
import sys

ROOT = Path(__file__).resolve().parent.parent
# The same 58-file Rust census as test-scratch.py; keep at least 50 files.
MINIMUM_FILES = 50
_unreachable = runpy.run_path(str(ROOT / "scripts/unreachable-modules.py"))
structural_source = _unreachable["structural_source"]
RAW_STRING_PREFIX = _unreachable["RAW_STRING_PREFIX"]
char_literal_end = _unreachable["_char_literal_end"]

HELPER = "SubcDaemonBinary"
SPAWN = re.compile(r"\bCommand::new\s*\(\s*&?\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)")
BINARY_LITERAL = re.compile(r"(?:.*/)?ck-subc(?:\.exe)?")


def string_literals(text):
    """Yield (offset, contents) for each string literal outside comments.

    Mirrors the lexing in `structural_source` (nested block comments, raw
    strings, char literals) so a quote inside a comment or a char literal is
    never mistaken for the start of a string.
    """
    index = 0
    block_depth = 0
    while index < len(text):
        if block_depth:
            if text.startswith("/*", index):
                block_depth += 1
                index += 2
            elif text.startswith("*/", index):
                block_depth -= 1
                index += 2
            else:
                index += 1
            continue
        if text.startswith("//", index):
            end = text.find("\n", index)
            index = len(text) if end < 0 else end
            continue
        if text.startswith("/*", index):
            block_depth = 1
            index += 2
            continue
        raw = RAW_STRING_PREFIX.match(text, index)
        if raw and (index == 0 or not (text[index - 1].isalnum() or text[index - 1] == "_")):
            terminator = '"' + raw.group("hashes")
            end = text.find(terminator, raw.end())
            end = len(text) if end < 0 else end
            yield index, text[raw.end():end]
            index = end + len(terminator)
            continue
        if text[index] == '"':
            end = index + 1
            while end < len(text) and text[end] != '"':
                end += 2 if text[end] == "\\" else 1
            yield index, text[index + 1:end]
            index = end + 1
            continue
        if text[index] == "'":
            end = char_literal_end(text, index)
            if end is not None:
                index = end
                continue
        index += 1


def violations(root=ROOT, minimum_files=MINIMUM_FILES):
    errors = []
    visited = 0
    for path in sorted((root / "crates").rglob("*.rs")):
        if path.is_symlink():
            continue
        relative = path.relative_to(root).as_posix()
        visited += 1
        text = path.read_text()
        for offset, contents in string_literals(text):
            if not BINARY_LITERAL.fullmatch(contents):
                continue
            line = text.count("\n", 0, offset) + 1
            errors.append(
                f"{relative}:{line}: names the subc daemon binary ({contents!r}); "
                f"build and spawn it through cortexkit_test_support::{HELPER}, "
                "whose command() fences the daemon out of the operator's "
                "production run/logs"
            )
        code = structural_source(text)
        for match in SPAWN.finditer(code):
            if "subc" not in match.group(1).lower():
                continue
            line = code.count("\n", 0, match.start()) + 1
            errors.append(
                f"{relative}:{line}: spawns the subc daemon with Command::new; "
                f"use cortexkit_test_support::{HELPER}::command, which fences "
                "the daemon out of the operator's production run/logs"
            )
    if visited < minimum_files:
        errors.append(f"tree scan read {visited} Rust files; expected at least {minimum_files}")
    return errors


def main():
    errors = violations()
    if errors:
        print("subc daemon env fence failed:", file=sys.stderr)
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"subc daemon env fence: every subc daemon is built and spawned through cortexkit_test_support::{HELPER}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
