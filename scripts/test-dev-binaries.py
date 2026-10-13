#!/usr/bin/env python3
"""Fence test launch sites away from production ck-* executable names."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parent.parent
# Motor has 69 eligible files: crates/**/*.rs plus TS, shell and Docker sources
# in the script/test trees, excluding dependency, build and fixture directories.
# Require at least 60 (about 85%) so a missing tree fails instead of silently
# weakening the test-executable fence; modest source shrinkage remains possible.
MINIMUM_FILES = 60
# Cargo artifact names are build inputs, not launch names. Every use must go
# through the scratch placement helper (cortexkit_test_support::ckdev_binary
# in motor) before entering a test module config.
CARGO_BINARY = re.compile(r'env!\s*\(\s*"CARGO_BIN_EXE_ck-[^"]+"\s*\)')
RAW_LAUNCH = re.compile(
    r'(?:Command::new|\blaunch|\bspawn(?:Sync)?|\bBun\.spawn)\s*\(\s*\[?\s*'
    r'["\x27`]([^"\x27`\n]*\bck-[^"\x27`\n]*)["\x27`]|'
    r'["\x27]?program["\x27]?\s*:\s*["\x27`]([^"\x27`\n]*\bck-[^"\x27`\n]*)["\x27`]'
)
CALL = re.compile(r'(?:Command::new|\blaunch|\bspawn(?:Sync)?|\bBun\.spawn(?:Sync)?)\s*\(')


def first_argument(source, start):
    """Keep nested path constructors; inspect only the program, not argv."""
    depth = 0
    quote = None
    index = start
    while index < len(source):
        char = source[index]
        if quote:
            if char == "\\":
                index += 2
                continue
            if char == quote:
                quote = None
        elif char in ('"', "'", '`'):
            quote = char
        elif char in "([{":
            depth += 1
        elif char in ")]}":
            if depth == 0:
                break
            depth -= 1
        elif char in ",;" and depth == 0:
            break
        index += 1
    return source[start:index].strip()


def program_literals(expression, preceding, depth=0):
    if re.search(r"\b(?:stage_test_binary|ckdev_binary|withTestBinary)\s*\(", expression):
        return []
    identifier = re.fullmatch(r"&?\s*([A-Za-z_]\w*)", expression)
    if identifier and depth < 4:
        name = re.escape(identifier.group(1))
        assignments = list(re.finditer(rf"\b(?:let(?:\s+mut)?|const|var)\s+{name}\s*=\s*", preceding))
        if assignments:
            assignment = assignments[-1]
            return program_literals(first_argument(preceding, assignment.end()), preceding[:assignment.start()], depth + 1)
    return re.findall(r'["\x27`]([^"\x27`\n]+)["\x27`]', expression)


def file_violations(source, path):
    errors = []
    for match in CALL.finditer(source):
        # Rust's tokio::spawn/JoinSet::spawn take futures, not programs.
        if Path(path).suffix == ".rs" and not match.group().startswith("Command::new"):
            continue
        start = match.end()
        # Node/Bun array form: only argv[0] is an executable name.
        array = re.match(r"\s*\[", source[start:])
        if array:
            start += array.end()
        expression = first_argument(source, start)
        for program in program_literals(expression, source[:match.start()]):
            if Path(program).name.startswith("ck-"):
                line = source.count("\n", 0, match.start()) + 1
                errors.append(f"{path}:{line}: test-spawned executable {program!r} must use ckdev-*")
    for match in RAW_LAUNCH.finditer(source):
        line = source.count("\n", 0, match.start()) + 1
        program = next(value for value in match.groups() if value is not None)
        if Path(program).name.startswith("ck-"):
            errors.append(f"{path}:{line}: test-spawned executable {program!r} must use ckdev-*")
    for match in CARGO_BINARY.finditer(source):
        # Include the opening placement call across rustfmt's multiline layout.
        prefix = source[max(0, match.start() - 160):match.start()]
        if not re.search(r"\b(?:stage_test_binary|ckdev_binary)\s*\([^;{}]*$", prefix):
            line = source.count("\n", 0, match.start()) + 1
            errors.append(f"{path}:{line}: stage Cargo fleet binaries with cortexkit_test_support::ckdev_binary before test use")
    if "Dockerfile" in Path(path).name:
        for number, line in enumerate(source.splitlines(), 1):
            if line.startswith("COPY --from=") and re.search(r"/usr/local/bin/ck-\S+\s*$", line):
                errors.append(f"{path}:{number}: Docker rig placement must use ckdev-*")
    return list(dict.fromkeys(errors))


def violations(root=ROOT, minimum_files=MINIMUM_FILES):
    files = set((root / "crates").rglob("*.rs"))
    for directory in ("tests", "script", "scripts", "packages", "modules", "rigs"):
        files.update(path for path in (root / directory).rglob("*")
                     if path.is_file() and (path.suffix in (".ts", ".sh") or "Dockerfile" in path.name)
                     and not any(part in ("node_modules", "dist", "fixtures") for part in path.parts))
    errors = []
    visited = 0
    for path in sorted(files):
        if path.is_symlink():
            continue
        visited += 1
        errors.extend(file_violations(path.read_text(), path.relative_to(root).as_posix()))
    if visited < minimum_files:
        errors.append(f"only scanned {visited} files; expected at least {minimum_files}")
    return errors


class TestDevBinaryNames(unittest.TestCase):
    def test_rejects_planted_test_spawn_violation(self):
        for source in (
            'Command::new("target/debug/ck-prefrontal-core").spawn()',
            'Command::new(Path::new("target/debug/ck-prefrontal-core")).spawn()',
            'let binary = root.join("ck-subc"); Command::new(&binary).spawn()',
            'const binary = join(root, "ck-motor"); spawn(binary, [])',
            'launch("ck-subc", [], "daemon")',
            'Bun.spawn(["/tmp/ck-motor", "--version"])',
            '{program: `/tmp/ck-${moduleID}`}',
            'PathBuf::from(env!("CARGO_BIN_EXE_ck-motor"))',
            'COPY --from=build /out/ck-subc /usr/local/bin/ck-subc',
        ):
            with self.subTest(source=source):
                self.assertTrue(file_violations(source, "tests/Dockerfile"), source)

    def test_accepts_dev_placements_and_build_inputs(self):
        for source in (
            'Command::new("/tmp/ckdev-subc").spawn()',
            'launch("ckdev-subc", [], "daemon")',
            'stage_test_binary(Path::new(env!("CARGO_BIN_EXE_ck-motor")))',
            'ckdev_binary(Path::new(env!("CARGO_BIN_EXE_ck-motor")))',
            'cortexkit_test_support::ckdev_binary(\n Path::new(env!("CARGO_BIN_EXE_ck-motor")))',
            'Command::new(cortexkit_test_support::ckdev_binary(Path::new("target/debug/ck-motor")))',
            'cargo build --bin ck-subc',
            'COPY --from=build /out/ck-subc /usr/local/bin/ckdev-subc',
        ):
            self.assertEqual(file_violations(source, "tests/Dockerfile"), [])

    def test_repository_test_spawns_use_dev_names(self):
        self.assertEqual(violations(), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
