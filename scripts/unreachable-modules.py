#!/usr/bin/env python3
"""Fail when a library module cannot be reached from any binary.

This catches a class that every other check misses. A subsystem built as a
series of slices can be internally complete and fully exercised by its own
tests while the slice that COMPOSES it into a running binary is never
dispatched. Nothing goes red: it compiles, its tests pass, and the functions
inside it really are called, by their siblings. The gap is one level up, so a
caller-based sweep over functions reports it as healthy.

Two refinements prevent false positives and false negatives:

  RE-EXPORTS COUNT AS WIRING. `pub use panel::{...}` in lib.rs flattens a
  module's symbols, and consumers then write `use prefrontal_routing_core::Foo`
  and never name `panel` at all. Those modules are still wired.

  ONE-HOP IMPORT IS THE WRONG QUESTION. Importing a module from another
  unreachable module does not connect it to a binary. A subtree
  can be fully interconnected and collectively dead. Only reachability from an
  entry point distinguishes wired from merely referenced.

A dormant module is allowed, but as a decision someone made rather than an
omission nobody noticed: record it in the allowlist with its reason.

Requires Python 3.11 or newer for standard-library TOML parsing.
"""

from __future__ import annotations

import sys

if sys.version_info < (3, 11):
    sys.exit("Python 3.11 or newer is required; run: python3.11 scripts/unreachable-modules.py")

import re
import subprocess
import tomllib
from collections import defaultdict, deque
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ALLOWLIST = ROOT / "scripts" / "unreachable-modules.allow"

@dataclass(frozen=True)
class CrateTargets:
    alias: str
    binaries: tuple[Path, ...]


def discover_crates() -> dict[str, CrateTargets]:
    """Read package identities and Cargo binary targets, including feature-gated ones."""
    crates = {}
    # Include independently built packages (for example platform agents) as well
    # as workspace members: the source census covers every crates/* package.
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        config = tomllib.loads(manifest.read_text())
        package = config.get("package")
        if package is None:
            continue
        directory = manifest.parent
        name = package["name"]
        explicit = config.get("bin", [])
        targets = {}
        for binary in explicit:
            target_name = binary.get("name", name)
            path = binary.get("path")
            if path is None:
                candidates = [directory / "src/bin" / f"{target_name}.rs",
                              directory / "src/bin" / target_name / "main.rs"]
                if target_name == name:
                    candidates.insert(0, directory / "src/main.rs")
                path = next((p for p in candidates if p.is_file()), None)
                if path is None:
                    raise ValueError(f"{manifest}: cannot find binary {target_name}")
            else:
                path = directory / path
            if not path.resolve().is_relative_to(ROOT):
                raise ValueError(f"{manifest}: binary path escapes repository: {path}")
            targets[target_name] = path.resolve()
        auto = package.get("autobins", not explicit or package.get("edition", "2015") != "2015")
        if auto:
            main = directory / "src/main.rs"
            if main.is_file():
                targets.setdefault(name, main.resolve())
            for entry in sorted((directory / "src/bin").glob("*.rs")):
                targets.setdefault(entry.stem, entry.resolve())
            for entry in sorted((directory / "src/bin").glob("*/main.rs")):
                targets.setdefault(entry.parent.name, entry.resolve())
        alias = config.get("lib", {}).get("name", name).replace("-", "_")
        crates[directory.name] = CrateTargets(alias, tuple(sorted(set(targets.values()))))
    return crates

MODULE_DECLARATION = re.compile(
    r"^\s*(?P<attrs>(?:#\s*\[[^]]*\]\s*)*)"
    r"(?P<visibility>pub(?:\s*\([^)]*\))?\s+)?"
    r"mod\s+(?P<name>[a-z_][a-z0-9_]*)\s*(?P<kind>[;{])"
)
ATTRIBUTE_LINE = re.compile(r"^\s*#\s*\[[^]]*\]\s*$")
CFG_TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
PATH_ATTRIBUTE = re.compile(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]')
FILE_CFG_TEST_ATTRIBUTE = re.compile(r"^\s*#!\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]", re.M)
RAW_STRING_PREFIX = re.compile(r'(?:br|r)(?P<hashes>#{0,255})"')
INCLUDE_CALL = re.compile(r"^\s*include!\s*\(")
INCLUDE_PATH = re.compile(r'^\s*include!\s*\(\s*"([^"\\]+)"\s*\)\s*;')


@dataclass(frozen=True)
class ModuleDeclaration:
    crate: str
    name: str
    declaring_file: Path
    line: int
    target: Path | None
    production_eligible: bool
    public: bool
    top_level: bool

    def site(self) -> str:
        return f"{self.declaring_file.relative_to(ROOT)}:{self.line}"


def _blank_non_newlines(chars: list[str], start: int, end: int) -> None:
    for index in range(start, end):
        if chars[index] != "\n":
            chars[index] = " "


def _char_literal_end(text: str, start: int) -> int | None:
    next_index = start + 1
    if next_index >= len(text) or text[next_index] == "\n":
        return None
    if text[next_index] != "\\":
        end = next_index + 1
        return end + 1 if end < len(text) and text[end] == "'" else None

    escaped = next_index + 1
    if escaped >= len(text) or text[escaped] == "\n":
        return None
    if text[escaped] == "u" and escaped + 1 < len(text) and text[escaped + 1] == "{":
        closing_brace = text.find("}", escaped + 2, escaped + 12)
        end = closing_brace + 1 if closing_brace >= 0 else -1
    else:
        end = escaped + 1
    return end + 1 if end >= 0 and end < len(text) and text[end] == "'" else None


def structural_source(text: str) -> str:
    """Blank comments and literals so braces describe Rust item structure."""
    chars = list(text)
    index = 0
    block_depth = 0
    while index < len(text):
        if block_depth:
            if text.startswith("/*", index):
                _blank_non_newlines(chars, index, index + 2)
                block_depth += 1
                index += 2
            elif text.startswith("*/", index):
                _blank_non_newlines(chars, index, index + 2)
                block_depth -= 1
                index += 2
            else:
                _blank_non_newlines(chars, index, index + 1)
                index += 1
            continue

        if text.startswith("//", index):
            end = text.find("\n", index)
            if end < 0:
                end = len(text)
            _blank_non_newlines(chars, index, end)
            index = end
            continue
        if text.startswith("/*", index):
            _blank_non_newlines(chars, index, index + 2)
            block_depth = 1
            index += 2
            continue

        raw = RAW_STRING_PREFIX.match(text, index)
        if raw and (index == 0 or not (text[index - 1].isalnum() or text[index - 1] == "_")):
            terminator = '"' + raw.group("hashes")
            end = text.find(terminator, raw.end())
            end = len(text) if end < 0 else end + len(terminator)
            _blank_non_newlines(chars, index, end)
            index = end
            continue

        if text[index] == '"':
            end = index + 1
            while end < len(text):
                if text[end] == "\\":
                    end += 2
                    continue
                end += 1
                if text[end - 1] == '"':
                    break
            _blank_non_newlines(chars, index, min(end, len(text)))
            index = end
            continue

        if text[index] == "'":
            end = _char_literal_end(text, index)
            if end is not None:
                _blank_non_newlines(chars, index, end)
                index = end
                continue

        index += 1
    return "".join(chars)


def production_source(text: str) -> str:
    """Keep code references, excluding comments, literals and cfg(test) items."""
    code = structural_source(text)
    if FILE_CFG_TEST_ATTRIBUTE.search(code):
        return ""
    chars = list(code)
    for match in CFG_TEST_ATTRIBUTE.finditer(code):
        # An attributed item ends at its balanced body or an un-nested
        # semicolon (for imports and out-of-line modules). Balance parentheses
        # and brackets too, so extra attributes and signatures stay attached.
        delimiters: list[str] = []
        end = match.end()
        while end < len(code):
            char = code[end]
            end += 1
            if char in "([{":
                delimiters.append(char)
            elif char in ")]}":
                if delimiters:
                    delimiters.pop()
                if char == "}" and not delimiters:
                    break
            elif char == ";" and not delimiters:
                break
        _blank_non_newlines(chars, match.start(), end)
    return "".join(chars)


def module_directory(path: Path) -> Path:
    if path.name in {"lib.rs", "main.rs", "mod.rs"}:
        return path.parent
    return path.parent / path.stem


def resolve_declaration(
    declaring_file: Path,
    name: str,
    path_attribute: str | None,
    inline_modules: tuple[str, ...],
    binary_entry: bool = False,
) -> Path | None:
    if path_attribute is not None:
        candidates = [declaring_file.parent / path_attribute]
    else:
        base = (declaring_file.parent if binary_entry else module_directory(declaring_file)).joinpath(*inline_modules)
        candidates = [base / f"{name}.rs", base / name / "mod.rs"]
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    return None


def declarations_in(path: Path, binary_entry: bool = False, crate: str | None = None) -> tuple[list[ModuleDeclaration], bool]:
    text = path.read_text(errors="ignore")
    structural = structural_source(text)
    file_test_only = bool(FILE_CFG_TEST_ATTRIBUTE.search(structural))
    declarations: list[ModuleDeclaration] = []
    pending_attributes: list[str] = []
    inline_modules: list[tuple[int, str, bool]] = []
    depth = 0
    if crate is None:
        crate = path.relative_to(ROOT / "crates").parts[0]

    original_lines = text.splitlines()
    structural_lines = join_wrapped_declarations(structural.splitlines())
    for line_number, (original, code) in enumerate(
        zip(original_lines, structural_lines), start=1
    ):
        while inline_modules and depth < inline_modules[-1][0]:
            inline_modules.pop()

        if ATTRIBUTE_LINE.match(code):
            pending_attributes.append(original)
            depth += code.count("{") - code.count("}")
            continue

        match = MODULE_DECLARATION.match(code)
        # include! compiles a source fragment in the enclosing scope without a
        # mod declaration. Inspect its path only after the structural scan has
        # proved this is code, not a comment or a string mentioning include!.
        included = INCLUDE_PATH.match(original) if INCLUDE_CALL.match(code) else None
        if match or included:
            attributes = "\n".join([*pending_attributes, original])
            cfg_test = bool(CFG_TEST_ATTRIBUTE.search(attributes))
            in_test_module = any(test_only for _, _, test_only in inline_modules)
            production_eligible = not (file_test_only or cfg_test or in_test_module)
            if match:
                kind = match.group("kind")
                name = match.group("name")
            else:
                assert included is not None
                kind = "include"
                name = Path(included.group(1)).stem
            target = None
            if included:
                candidate = path.parent / included.group(1)
                if candidate.is_file():
                    target = candidate.resolve()
            elif kind == ";":
                path_match = PATH_ATTRIBUTE.search(attributes)
                target = resolve_declaration(
                    path,
                    name,
                    path_match.group(1) if path_match else None,
                    tuple(module for _, module, _ in inline_modules),
                    binary_entry,
                )
            declaration = ModuleDeclaration(
                crate=crate,
                name=name,
                declaring_file=path,
                line=line_number,
                target=target,
                production_eligible=production_eligible,
                public=bool(match and (match.group("visibility") or "").strip() == "pub"),
                top_level=not inline_modules,
            )
            declarations.append(declaration)
            if kind == "{":
                inline_modules.append((depth + 1, name, not production_eligible))
            pending_attributes.clear()
        elif code.strip():
            pending_attributes.clear()

        depth += code.count("{") - code.count("}")

    return declarations, file_test_only


def join_wrapped_declarations(lines: list[str]) -> list[str]:
    """Fold a `mod` declaration that Rust allows to span lines back onto one.

    The scan below matches one declaration per physical line, but Rust permits
    whitespace and comments between `mod` and its name, so a wrapped
    declaration matched neither half: the gate then reported an already-wired
    module as undeclared and advised declaring or deleting it (Loop R121). The
    consumed line is blanked rather than removed so line numbers and brace
    depth keep counting as before.
    """
    folded = list(lines)
    for index in range(len(folded) - 1):
        if not re.search(r"\bmod\s*$", folded[index]):
            continue
        successor = index + 1
        while successor < len(folded) and not folded[successor].strip():
            successor += 1
        if successor >= len(folded):
            continue
        if re.match(r"^\s*[a-z_][a-z0-9_]*\s*[;{]", folded[successor]):
            folded[index] = f"{folded[index].rstrip()} {folded[successor].strip()}"
            folded[successor] = ""
    return folded


def collect_declarations(
    sources: list[Path], binary_entries: set[Path], source_crates: dict[Path, str],
) -> tuple[list[ModuleDeclaration], set[Path]]:
    declarations: list[ModuleDeclaration] = []
    test_only_files: set[Path] = set()
    for path in sources:
        found, file_test_only = declarations_in(path, path in binary_entries, source_crates[path])
        declarations.extend(found)
        if file_test_only:
            test_only_files.add(path.resolve())
    return declarations, test_only_files


def tracked_sources(binary_owners: dict[Path, str]) -> tuple[list[Path], dict[Path, str]]:
    out = subprocess.run(
        ["git", "ls-files", ":(glob)**/*.rs"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    tracked = {(ROOT / line).resolve() for line in out.stdout.splitlines() if line}
    # Keep the ordinary src census, not standalone compile probes under tests/.
    source_crates = {}
    for path in tracked:
        if path.is_relative_to(ROOT / "crates"):
            parts = path.relative_to(ROOT / "crates").parts
            if len(parts) >= 3 and parts[1] == "src":
                source_crates[path] = parts[0]
    sources = set(source_crates)
    queue = deque(binary_owners.items())
    visited = set()
    while queue:
        path, crate = queue.popleft()
        if path in visited:
            continue
        visited.add(path)
        if path not in tracked:
            raise ValueError(f"binary source is missing or not tracked by git: {path}")
        sources.add(path)
        source_crates[path] = crate
        declarations, _ = declarations_in(path, path in binary_owners, crate)
        for declaration in declarations:
            if declaration.production_eligible and declaration.target is not None:
                queue.append((declaration.target, crate))
    return sorted(sources), source_crates





def declared_modules(
    crate: str, declarations: list[ModuleDeclaration]
) -> set[str]:
    """Production top-level modules exported by a library crate."""
    lib = (ROOT / "crates" / crate / "src" / "lib.rs").resolve()
    return {
        declaration.name
        for declaration in declarations
        if declaration.declaring_file == lib
        and declaration.public
        and declaration.top_level
        and declaration.production_eligible
    }


def reexported_modules(crate: str) -> set[str]:
    """Modules whose symbols lib.rs flattens: consumers never name them."""
    lib = ROOT / "crates" / crate / "src" / "lib.rs"
    if not lib.exists():
        return set()
    body = production_source(lib.read_text(errors="ignore"))
    return set(re.findall(r"^pub use ([a-z_][a-z0-9_]*)::", body, re.M))


def braced_group_modules(text: str, open_brace: int) -> set[str]:
    """Modules named at the top level of the `{...}` group opening at open_brace.

    In `use crate_name::{Item, module::{a, b}, other::c}` the modules are
    `module` and `other`; names nested deeper belong to those modules.
    """
    modules: set[str] = set()
    depth = 0
    index = open_brace
    item_start = True
    while index < len(text):
        char = text[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                break
        elif depth == 1 and char == ",":
            item_start = True
        elif depth == 1 and item_start and not char.isspace():
            name = re.match(r"([a-z_][a-z0-9_]*)\b", text[index:])
            if name:
                modules.add(name.group(1))
                index += len(name.group(0)) - 1
            # Renamed imports do not wire a module named after their alias.
            item_start = False
        index += 1
    return modules


def references(text: str, crate: str, aliases: dict[str, str]) -> set[tuple[str, str]]:
    """Named (crate, module) pairs; an empty module denotes a crate's root."""
    found: set[tuple[str, str]] = set()
    # prefrontal_core::org::... and prefrontal_core::org;
    for alias, other in aliases.items():
        if re.search(rf"\b{alias}::", text):
            found.add((other, ""))
        for module in re.findall(rf"\b{alias}::([a-z_][a-z0-9_]*)", text):
            found.add((other, module))
        # `use prefrontal_core::{Item, org::{...}}` names `org` only inside the
        # braces, so collect modules from a braced group too.
        for group in re.finditer(rf"\b{alias}::\s*\{{", text):
            for module in braced_group_modules(text, group.end() - 1):
                found.add((other, module))
    # crate::org::..., self::org::..., super::org::...
    for module in re.findall(r"\b(?:crate|self|super)::([a-z_][a-z0-9_]*)", text):
        found.add((crate, module))
    for group in re.finditer(r"\b(?:crate|self|super)::\s*\{", text):
        for module in braced_group_modules(text, group.end() - 1):
            found.add((crate, module))
    # bare `use org::...` inside the declaring crate
    for module in re.findall(r"^\s*use ([a-z_][a-z0-9_]*)::", text, re.M):
        found.add((crate, module))
    return found


def local_module_path_references(
    text: str, crate: str, modules: set[str]
) -> set[tuple[str, str]]:
    """Find declared root modules used directly in an in-crate path expression."""
    return {
        (crate, module)
        for module in modules
        if re.search(rf"\b{re.escape(module)}\s*::", text)
    }


def allowlisted() -> dict[str, str]:
    if not ALLOWLIST.exists():
        return {}
    entries: dict[str, str] = {}
    for line in ALLOWLIST.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, _, why = line.partition("#")
        entries[key.strip()] = why.strip()
    return entries


def undeclared_module_files(
    sources: list[Path], declarations: list[ModuleDeclaration], binary_entries: set[Path], source_crates: dict[Path, str]
) -> list[str]:
    """Return tracked Rust files with no resolved declaration or include in their crate."""
    declared_by_crate: dict[str, set[Path]] = defaultdict(set)
    for declaration in declarations:
        if declaration.target is None:
            continue
        target_crate = source_crates.get(declaration.target)
        if target_crate is not None and target_crate == declaration.crate:
            declared_by_crate[target_crate].add(declaration.target)

    missing: list[str] = []
    for path in sources:
        if path in binary_entries:
            continue
        crate = source_crates[path]
        directory = ROOT / "crates" / crate
        crate_relative = path.relative_to(directory) if path.is_relative_to(directory) else path.relative_to(ROOT)
        if crate_relative in {Path("src/lib.rs"), Path("src/main.rs")}:
            continue
        # Cargo discovers `src/bin/<name>.rs` and `src/bin/<name>/main.rs` as
        # binary targets without a declaration, the same way it finds main.rs.
        if is_auto_binary_entry(crate_relative):
            continue
        if path.resolve() not in declared_by_crate.get(crate, set()):
            missing.append(str(path.relative_to(ROOT)))
    return missing


def is_auto_binary_entry(crate_relative: Path) -> bool:
    parts = crate_relative.parts
    if len(parts) == 3 and parts[:2] == ("src", "bin") and parts[2].endswith(".rs"):
        return True
    return len(parts) == 4 and parts[:2] == ("src", "bin") and parts[3] == "main.rs"


def duplicate_declarations(
    declarations: list[ModuleDeclaration],
) -> list[tuple[Path, list[ModuleDeclaration]]]:
    by_target: dict[Path, list[ModuleDeclaration]] = defaultdict(list)
    for declaration in declarations:
        if declaration.target is not None:
            by_target[declaration.target].append(declaration)
    return sorted(
        (
            (
                target,
                sorted(
                    found,
                    key=lambda item: (
                        str(item.declaring_file.relative_to(ROOT)),
                        item.line,
                    ),
                ),
            )
            for target, found in by_target.items()
            if len(found) > 1
        ),
        key=lambda item: str(item[0]),
    )


def production_subtree(
    entry: Path,
    declarations_by_file: dict[Path, list[ModuleDeclaration]],
    test_only_files: set[Path],
) -> set[Path]:
    """Files Rust can include from entry without enabling its test cfg."""
    included: set[Path] = set()
    queue = deque([entry.resolve()])
    while queue:
        path = queue.popleft()
        if path in included or path in test_only_files or not path.is_file():
            continue
        included.add(path)
        for declaration in declarations_by_file.get(path, []):
            if (
                declaration.production_eligible
                and declaration.target is not None
                and declaration.target not in included
            ):
                queue.append(declaration.target)
    return included


def main() -> int:
    try:
        targets = discover_crates()
        binary_owners = {entry: crate for crate, target in targets.items() for entry in target.binaries}
        binary_entries = set(binary_owners)
        sources, source_crates = tracked_sources(binary_owners)
    except (OSError, ValueError) as error:
        print(f"cannot discover Cargo binary targets: {error}", file=sys.stderr)
        return 1
    aliases = {target.alias: crate for crate, target in targets.items()}
    source_text = {path: path.read_text(errors="ignore") for path in sources}
    reference_text = {
        path: production_source(text) for path, text in source_text.items()
    }
    declarations, test_only_files = collect_declarations(sources, binary_entries, source_crates)

    duplicates = duplicate_declarations(declarations)
    if duplicates:
        print("module files declared more than once:", file=sys.stderr)
        for path, found in duplicates:
            print(
                f"  {path.relative_to(ROOT)} declared twice; a file compiles once "
                "per declaration and its tests run once per path",
                file=sys.stderr,
            )
            for declaration in found:
                print(f"    {declaration.site()}", file=sys.stderr)
        return 1

    undeclared = undeclared_module_files(sources, declarations, binary_entries, source_crates)
    if undeclared:
        print(
            "module files with no `mod` declaration or include! anywhere in their crate:",
            file=sys.stderr,
        )
        for entry in undeclared:
            print(f"  {entry}", file=sys.stderr)
        print(
            "\nThese are outside the declaration graph entirely: the reachability"
            "\nwalk cannot even see them, and they compile only if a test names"
            "\nthem via #[path]. Either declare/include the source or delete the file.",
            file=sys.stderr,
        )
        return 1

    declarations_by_file: dict[Path, list[ModuleDeclaration]] = defaultdict(list)
    for declaration in declarations:
        declarations_by_file[declaration.declaring_file].append(declaration)

    # Every library node owns the references in its production module subtree.
    edges: dict[tuple[str, str], set[tuple[str, str]]] = {}
    node_files: dict[tuple[str, str], set[Path]] = {}
    crates = set(source_crates.values())
    for crate in crates:
        lib = (ROOT / "crates" / crate / "src" / "lib.rs").resolve()
        # Naming any library symbol reaches its root's own production code,
        # even through another library. Declarations alone do not reach every
        # module: only root references and re-exports add outgoing edges.
        if lib in reference_text and lib not in test_only_files:
            root_modules = {
                declaration.name
                for declaration in declarations_by_file.get(lib, [])
                if declaration.top_level
                and declaration.production_eligible
            }
            root_text = reference_text[lib]
            edges[(crate, "")] = (
                references(root_text, crate, aliases)
                | {(crate, module) for module in reexported_modules(crate)}
                | local_module_path_references(root_text, crate, root_modules)
            )
        for declaration in declarations_by_file.get(lib, []):
            if not (
                declaration.top_level
                and declaration.production_eligible
            ):
                continue
            node = (crate, declaration.name)
            files = (
                production_subtree(
                    declaration.target, declarations_by_file, test_only_files
                )
                if declaration.target is not None
                else set()
            )
            node_files[node] = files
            for path in files:
                if path not in source_text:
                    # Declared in a tracked file but not itself tracked: a module added
                    # without `git add`. Name it instead of dying on the lookup.
                    print(f"declared module file is not tracked by git: {path.relative_to(ROOT)}", file=sys.stderr)
                    return 1
                edges.setdefault(node, set()).update(
                    references(reference_text[path], crate, aliases)
                )

    # A binary contributes roots only from files reachable from its entry in a
    # production build. Test-only declarations and their descendants stay out.
    roots: set[tuple[str, str]] = set()
    for crate, target in targets.items():
        for entry in target.binaries:
            included = production_subtree(entry, declarations_by_file, test_only_files)
            for path in included:
                roots |= references(reference_text[path], crate, aliases)

    # Breadth-first over the module graph from the binaries.
    reachable: set[tuple[str, str]] = set()
    queue = deque(roots)
    while queue:
        node = queue.popleft()
        if node in reachable:
            continue
        reachable.add(node)
        for nxt in edges.get(node, ()):  # noqa: B007
            if nxt not in reachable:
                queue.append(nxt)

    allow = allowlisted()

    # The allowlist doubles as a standing mutation fixture: every entry is a
    # KNOWN-true-positive the detector must keep finding. If an allowlisted
    # module shows up as reachable, one of two things happened and both demand
    # a human: the module got wired (remove the entry, it graduated) or the
    # detector itself broke in the direction that clears everything, which no
    # green run would otherwise reveal. A detector that finds nothing and a
    # detector that cannot find anything print the same clean summary; this is
    # the only line that tells them apart, and it runs on every gate pass
    # rather than the one time someone thought to mutation-prove it.
    declared_by_crate = {
        crate: declared_modules(crate, declarations)
        for crate in crates
    }
    fixture_broken: list[str] = []
    for key in allow:
        crate, _, module = key.partition("/")
        if module not in declared_by_crate.get(crate, set()):
            continue  # the module was deleted; the stale entry is harmless
        if (crate, module) in reachable:
            fixture_broken.append(key)
    if fixture_broken:
        print(
            "allowlisted modules are now REACHABLE: "
            "either they got wired (remove the entry: it graduated) or the "
            "detector broke in the direction that clears everything:",
            file=sys.stderr,
        )
        for key in fixture_broken:
            print(f"  {key}", file=sys.stderr)
        return 1

    offenders: list[tuple[str, int, str]] = []
    for crate in sorted(crates):
        for module in sorted(declared_by_crate.get(crate, set())):
            if (crate, module) in reachable:
                continue
            key = f"{crate}/{module}"
            if key in allow:
                continue
            files = node_files.get((crate, module), set())
            lines = sum(len(source_text[path].splitlines()) for path in files)
            offenders.append((key, lines, f"{len(files)} file(s)"))

    if offenders:
        print(
            "unreachable modules: declared, but no binary can reach them",
            file=sys.stderr,
        )
        for key, lines, detail in offenders:
            print(f"  {key}  ({lines} lines, {detail})", file=sys.stderr)
        print(file=sys.stderr)
        print(
            "These compile and may pass their own tests while being unable to\n"
            "execute, because nothing composes them into a running binary.\n\n"
            f"Either wire it, or record the decision in {ALLOWLIST.relative_to(ROOT)}:\n"
            "  <crate>/<module>  # why it is intentionally dormant",
            file=sys.stderr,
        )
        return 1

    for key, reason in allow.items():
        crate, _, module = key.partition("/")
        if module in declared_by_crate.get(crate, set()):
            print(f"    allowlisted unreachable module {key}: {reason}")
    print("    every declared module is reachable from a binary")
    return 0


if __name__ == "__main__":
    sys.exit(main())
