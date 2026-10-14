#!/usr/bin/env python3
"""Fixture tests for the Rust module reachability census."""
from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "unreachable-modules.py"


class UnreachableModulesTest(unittest.TestCase):
    def run_gate(
        self, files: dict[str, str], allowlist: str = ""
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory(prefix="unreachable-modules-") as temporary:
            fixture = Path(temporary)
            # Synthetic library hops exercise transitive wiring without adding
            # nonexistent crate aliases to the production motor gate.
            script = SCRIPT.read_text().replace(
                'if __name__ == "__main__":',
                'CRATE_ALIASES.update({"example_bridge": "example-bridge", '
                '"example_transitive": "example-transitive"})\n'
                'if __name__ == "__main__":',
            )
            self.write(fixture, "scripts/unreachable-modules.py", script)
            self.write(fixture, "scripts/unreachable-modules.allow", allowlist)
            for relative, contents in files.items():
                self.write(fixture, relative, contents)
            subprocess.run(
                ["git", "init", "-q"], cwd=fixture, check=True, capture_output=True
            )
            subprocess.run(
                ["git", "add", "."], cwd=fixture, check=True, capture_output=True
            )
            return subprocess.run(
                ["python3", "scripts/unreachable-modules.py"],
                cwd=fixture,
                text=True,
                capture_output=True,
                check=False,
            )

    @staticmethod
    def write(root: Path, relative: str, contents: str) -> None:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)

    @staticmethod
    def wiring_fixture(declaration: str) -> dict[str, str]:
        return {
            "crates/motor-protocol/src/lib.rs": "pub mod feature;\n",
            "crates/motor-protocol/src/feature.rs": "pub fn run() {}\n",
            "crates/motor-module/src/main.rs": (
                f"{declaration}\nfn main() {{}}\n"
            ),
            "crates/motor-module/src/bridge.rs": (
                "pub fn run() { motor_protocol::feature::run(); }\n"
            ),
        }

    def test_test_only_wiring_is_rejected(self) -> None:
        test_only = self.run_gate(
            self.wiring_fixture("#[cfg(test)]\nmod bridge;")
        )
        self.assertNotEqual(test_only.returncode, 0, test_only.stdout + test_only.stderr)
        self.assertIn("motor-protocol/feature", test_only.stderr)

        enclosed = self.wiring_fixture(
            '#[cfg(test)]\nmod tests {\n    #[path = "bridge.rs"]\n    mod bridge;\n}'
        )
        enclosed_result = self.run_gate(enclosed)
        self.assertNotEqual(
            enclosed_result.returncode,
            0,
            enclosed_result.stdout + enclosed_result.stderr,
        )
        self.assertIn("motor-protocol/feature", enclosed_result.stderr)

        file_test_only = self.wiring_fixture("mod bridge;")
        file_test_only["crates/motor-module/src/bridge.rs"] = (
            "#![cfg(test)]\n"
            "pub fn run() { motor_protocol::feature::run(); }\n"
        )
        file_result = self.run_gate(file_test_only)
        self.assertNotEqual(
            file_result.returncode, 0, file_result.stdout + file_result.stderr
        )
        self.assertIn("motor-protocol/feature", file_result.stderr)

        production = self.run_gate(self.wiring_fixture("mod bridge;"))
        self.assertEqual(production.returncode, 0, production.stdout + production.stderr)
        self.assertIn("every declared module is reachable", production.stdout)

    def test_module_named_inside_a_braced_use_group_is_reachable(self) -> None:
        # `use motor_protocol::{Item, feature::{run}}` names `feature` only
        # inside the braces; the census must still count it as wired.
        files = self.wiring_fixture("mod bridge;")
        files["crates/motor-module/src/bridge.rs"] = (
            "use motor_protocol::{\n    Item,\n    feature::{run},\n};\n"
            "pub fn call() { run(); }\n"
        )
        result = self.run_gate(files)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

        # A name nested a level deeper belongs to the outer module, not to the
        # crate root, so it must not wire a same-named root module.
        files["crates/motor-protocol/src/lib.rs"] = "pub mod feature;\npub mod run;\n"
        files["crates/motor-protocol/src/run.rs"] = "pub fn go() {}\n"
        files["crates/motor-module/src/bridge.rs"] = (
            "use motor_protocol::{feature::{run::go}};\npub fn call() { go(); }\n"
        )
        nested = self.run_gate(files)
        self.assertNotEqual(nested.returncode, 0, nested.stdout + nested.stderr)
        self.assertIn("motor-protocol/run", nested.stderr)

    def test_binary_links_its_own_library_tree(self) -> None:
        # motor-module ships a library and a binary. A binary's reference to
        # its own library must seed that library's production wiring too.
        def fixture(lib_declaration: str) -> dict[str, str]:
            return {
                "crates/motor-protocol/src/lib.rs": "pub mod feature;\n",
                "crates/motor-protocol/src/feature.rs": "pub fn run() {}\n",
                "crates/motor-module/src/main.rs": (
                    "use motor_module::runtime;\nfn main() { runtime::run(); }\n"
                ),
                "crates/motor-module/src/lib.rs": (
                    f"pub mod runtime;\n{lib_declaration}\n"
                ),
                "crates/motor-module/src/runtime.rs": "pub fn run() {}\n",
                "crates/motor-module/src/bridge.rs": (
                    "pub fn run() { motor_protocol::feature::run(); }\n"
                ),
            }

        linked = self.run_gate(fixture("pub mod bridge;"))
        self.assertEqual(linked.returncode, 0, linked.stdout + linked.stderr)
        self.assertIn("every declared module is reachable", linked.stdout)

        test_only = self.run_gate(fixture("#[cfg(test)]\nmod bridge;"))
        self.assertNotEqual(test_only.returncode, 0, test_only.stdout + test_only.stderr)
        self.assertIn("motor-protocol/feature", test_only.stderr)

    @staticmethod
    def library_root_fixture(root_code: str) -> dict[str, str]:
        return {
            "crates/motor-protocol/src/lib.rs": "pub mod feature;\n",
            "crates/motor-protocol/src/feature.rs": "pub fn run() {}\n",
            "crates/example-bridge/src/lib.rs": root_code,
            "crates/motor-module/src/main.rs": (
                "fn main() { example_bridge::run(); }\n"
            ),
        }

    def test_library_root_references_are_reachable(self) -> None:
        result = self.run_gate(self.library_root_fixture(
            "pub fn run() { motor_protocol::feature::run(); }\n"
        ))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

    def test_library_root_references_are_transitive(self) -> None:
        files = self.library_root_fixture(
            "pub fn run() { motor_protocol::feature::run(); }\n"
        )
        files.update({
            "crates/example-transitive/src/lib.rs": "pub mod bridge;\n",
            "crates/example-transitive/src/bridge.rs": (
                "pub fn run() { example_bridge::run(); }\n"
            ),
            "crates/motor-module/src/main.rs": (
                "fn main() { example_transitive::bridge::run(); }\n"
            ),
        })
        result = self.run_gate(files)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

    def test_library_root_reexports_remain_reachable(self) -> None:
        files = self.library_root_fixture(
            "pub fn run() { motor_protocol::run(); }\n"
        )
        files["crates/motor-protocol/src/lib.rs"] += "pub use feature::run;\n"
        result = self.run_gate(files)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

    def test_test_only_library_root_references_are_rejected(self) -> None:
        for test_code in [
            "#[cfg(test)]\nfn check() { motor_protocol::feature::run(); }\n",
            "#[cfg(test)] mod tests {\n"
            "    fn check() { motor_protocol::feature::run(); }\n}\n",
            "#[cfg(test)]\nuse motor_protocol::feature;\n",
            "#[cfg(test)]\npub use motor_protocol::feature;\n",
            "struct Store;\n#[cfg(test)]\n#[allow(dead_code)]\nimpl Store {\n"
            "    fn check() { motor_protocol::feature::run(); }\n}\n",
        ]:
            with self.subTest(test_code=test_code):
                result = self.run_gate(self.library_root_fixture(
                    test_code + "pub fn run() {}\n"
                ))
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("motor-protocol/feature", result.stderr)

    def test_library_root_production_after_test_item_remains_reachable(self) -> None:
        # Literal and comment braces must not close the test module early or
        # swallow the production function that follows it.
        result = self.run_gate(self.library_root_fixture(
            "#[cfg(test)]\nmod tests {\n"
            '    const PROSE: &str = r#" } { "#;\n'
            "    // }\n"
            "    fn check() {}\n}\n"
            "pub fn run() { motor_protocol::feature::run(); }\n"
        ))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

    def test_library_root_local_module_path_expressions_are_reachable(self) -> None:
        result = self.run_gate(
            {
                "crates/example-bridge/src/lib.rs": """#[cfg(test)]
mod tests {
    const PROSE: &str = r#" } { "#;
    fn check(conn: &()) { super::elicitation::test_only(conn); }
}
pub mod elicitation;
pub struct Store;
impl Store {
    pub fn open(store: &Store) {
        let _sql = r#"CREATE TABLE records (
            payload TEXT DEFAULT '{}',
            note TEXT DEFAULT 'quoted'
        );
        -- braces and quotes in SQL must not hide the call: { "quoted" }
        -- elicitation::sql_only(); is text, not production wiring
        "#;
        let _options = ["atomic", "durable", "shared"];
        let _namespace = "elicitation";
        if _options.is_empty() {
            return;
        }
        let _opening = store
            .refuse_store_ahead(|| Ok(()))
            .with_conn(|conn| {
                let _transaction_label = "opening";
                conn.execute_batch("PRAGMA foreign_keys = ON;");
                elicitation::deletion_open(conn)
            });
    }
}
""",
                "crates/example-bridge/src/elicitation.rs": (
                    "pub fn deletion_open(_conn: &()) {}\n"
                ),
                "crates/motor-module/src/main.rs": (
                    "fn main() { example_bridge::run(); }\n"
                ),
            }
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

    def test_library_root_non_code_references_are_rejected(self) -> None:
        result = self.run_gate(self.library_root_fixture(
            "// motor_protocol::feature::run();\n"
            'const PROSE: &str = "motor_protocol::feature::run();";\n'
            "pub fn run() {}\n"
        ))
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("motor-protocol/feature", result.stderr)

    def test_library_named_only_by_test_code_is_not_reached(self) -> None:
        files = self.library_root_fixture(
            "pub fn run() { motor_protocol::feature::run(); }\n"
        )
        files["crates/motor-module/src/main.rs"] = (
            "#[cfg(test)]\nfn check() { example_bridge::run(); }\n"
            "fn main() {}\n"
        )
        result = self.run_gate(files)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("motor-protocol/feature", result.stderr)

    def test_unlinked_library_root_does_not_seed_references(self) -> None:
        files = self.library_root_fixture(
            "pub fn run() { motor_protocol::feature::run(); }\n"
        )
        files["crates/motor-module/src/main.rs"] = "fn main() {}\n"
        result = self.run_gate(files)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("motor-protocol/feature", result.stderr)

    def test_library_root_does_not_reach_all_declared_modules(self) -> None:
        files = self.library_root_fixture(
            "pub fn run() { motor_protocol::feature::run(); }\n"
        )
        files["crates/example-bridge/src/lib.rs"] += "pub mod dormant;\n"
        files["crates/example-bridge/src/dormant.rs"] = "pub fn run() {}\n"
        result = self.run_gate(files, "example-bridge/dormant # not wired\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "allowlisted unreachable module example-bridge/dormant",
            result.stdout,
        )

    def test_allowlisted_reachable_module_is_refused(self) -> None:
        result = self.run_gate(self.library_root_fixture(
            "pub fn run() { motor_protocol::feature::run(); }\n"
        ), "motor-protocol/feature # not wired\n")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("allowlisted modules are now REACHABLE", result.stderr)
        self.assertIn("motor-protocol/feature", result.stderr)
    def test_motor_binary_links_its_own_production_library(self) -> None:
        files = {
            "crates/motor-protocol/src/lib.rs": "pub mod feature;\n",
            "crates/motor-protocol/src/feature.rs": "pub fn run() {}\n",
            "crates/motor-module/src/main.rs": (
                "use motor_module::module;\nfn main() { module::run(); }\n"
            ),
            "crates/motor-module/src/lib.rs": "pub mod module;\n",
            "crates/motor-module/src/module.rs": (
                "pub fn run() { motor_protocol::feature::run(); }\n"
            ),
        }
        linked = self.run_gate(files)
        self.assertEqual(linked.returncode, 0, linked.stdout + linked.stderr)
        self.assertIn("every declared module is reachable", linked.stdout)

        files["crates/motor-module/src/lib.rs"] = "#[cfg(test)]\npub mod module;\n"
        test_only = self.run_gate(files)
        self.assertNotEqual(test_only.returncode, 0, test_only.stdout + test_only.stderr)
        self.assertIn("motor-protocol/feature", test_only.stderr)

        files["crates/motor-module/src/lib.rs"] = "pub mod module;\n"
        files["crates/motor-module/src/main.rs"] = "fn main() {}\n"
        unlinked = self.run_gate(files)
        self.assertNotEqual(unlinked.returncode, 0, unlinked.stdout + unlinked.stderr)
        self.assertIn("motor-protocol/feature", unlinked.stderr)

    def check_vm_binary_links_its_own_production_library(self, package: str) -> None:
        alias = package.replace("-", "_")
        files = {
            "crates/motor-protocol/src/lib.rs": "pub mod feature;\n",
            "crates/motor-protocol/src/feature.rs": "pub fn run() {}\n",
            f"crates/{package}/src/main.rs": (
                f"fn main() {{ {alias}::runtime::run(); }}\n"
            ),
            f"crates/{package}/src/lib.rs": "pub mod runtime;\n",
            f"crates/{package}/src/runtime.rs": (
                "pub fn run() { motor_protocol::feature::run(); }\n"
            ),
        }
        linked = self.run_gate(files)
        self.assertEqual(linked.returncode, 0, linked.stdout + linked.stderr)
        self.assertIn("every declared module is reachable", linked.stdout)

        for main in [
            f"#[cfg(test)]\nfn test_only() {{ {alias}::runtime::run(); }}\nfn main() {{}}\n",
            "fn main() {}\n",
        ]:
            with self.subTest(package=package, main=main):
                files[f"crates/{package}/src/main.rs"] = main
                unlinked = self.run_gate(files)
                self.assertNotEqual(
                    unlinked.returncode, 0, unlinked.stdout + unlinked.stderr
                )
                self.assertIn("motor-protocol/feature", unlinked.stderr)

    def test_vm_guest_binary_links_its_own_production_library(self) -> None:
        self.check_vm_binary_links_its_own_production_library("motor-vm-guest")

    def test_vm_loader_binary_links_its_own_production_library(self) -> None:
        self.check_vm_binary_links_its_own_production_library("motor-vm-loader")

    def test_duplicate_declaration_is_refused(self) -> None:
        result = self.run_gate(
            {
                "crates/motor-module/src/main.rs": (
                    "mod hires;\n"
                    '#[path = "hires.rs"]\n'
                    "mod host_hires;\n"
                    "fn main() {}\n"
                ),
                "crates/motor-module/src/hires.rs": "pub fn run() {}\n",
            }
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("crates/motor-module/src/hires.rs", result.stderr)
        self.assertIn("crates/motor-module/src/main.rs:1", result.stderr)
        self.assertIn("crates/motor-module/src/main.rs:3", result.stderr)
        self.assertIn(
            "declared twice; a file compiles once per declaration and its tests run once per path",
            result.stderr,
        )

    def test_path_declaration_resolves_file_and_seeds_references(self) -> None:
        result = self.run_gate(
            {
                "crates/motor-protocol/src/lib.rs": "pub mod feature;\n",
                "crates/motor-protocol/src/feature.rs": "pub fn run() {}\n",
                "crates/motor-module/src/main.rs": (
                    '#[path = "sub/x.rs"]\nmod bridge;\nfn main() {}\n'
                ),
                "crates/motor-module/src/sub/x.rs": (
                    "pub fn run() { motor_protocol::feature::run(); }\n"
                ),
            }
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

    def test_undeclared_file_is_refused(self) -> None:
        result = self.run_gate(
            {
                "crates/motor-module/src/main.rs": "fn main() {}\n",
                "crates/motor-module/src/orphan.rs": "pub fn run() {}\n",
            }
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("no `mod` declaration", result.stderr)
        self.assertIn("crates/motor-module/src/orphan.rs", result.stderr)

    def test_include_resolves_file_without_a_mod_declaration(self) -> None:
        files = self.wiring_fixture('include!("bridge.rs");')
        result = self.run_gate(files)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("every declared module is reachable", result.stdout)

        for source in [
            '// include!("bridge.rs");\nfn main() {}\n',
            'const PROSE: &str = r#"\ninclude!("bridge.rs");\n"#;\nfn main() {}\n',
        ]:
            files["crates/motor-module/src/main.rs"] = source
            not_code = self.run_gate(files)
            self.assertNotEqual(not_code.returncode, 0, not_code.stdout + not_code.stderr)
            self.assertIn("no `mod` declaration", not_code.stderr)

    def test_test_only_include_does_not_wire_production(self) -> None:
        result = self.run_gate(self.wiring_fixture(
            '#[cfg(test)]\nmod tests {\n    include!("bridge.rs");\n}'
        ))
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("no `mod` declaration", result.stderr)
        self.assertIn("motor-protocol/feature", result.stderr)

    def test_allowlisted_unreachable_module_prints_reason(self) -> None:
        reason = "waiting for a production composition owner"
        result = self.run_gate(
            {
                "crates/motor-protocol/src/lib.rs": "pub mod dormant;\n",
                "crates/motor-protocol/src/dormant.rs": "pub fn run() {}\n",
                "crates/motor-module/src/main.rs": "fn main() {}\n",
            },
            f"motor-protocol/dormant  # {reason}\n",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(reason, result.stdout)
        self.assertIn("every declared module is reachable", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
