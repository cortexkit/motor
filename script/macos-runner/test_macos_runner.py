#!/usr/bin/env python3
"""Portable contract tests for the macOS image, installer and parity boundary."""

import copy
from contextlib import nullcontext
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import plistlib
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import unittest
from types import SimpleNamespace
from typing import Any, Optional
from unittest.mock import ANY, patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import download
import guest
import guest_services
import image
import install
import parity
import pinlock

SERVER = json.loads((HERE.parent / "runner/toolchain.json").read_text())
DATA_UUID = "12345678-1234-1234-1234-123456789ABC"
NEXT_BOOT = "23456789-1234-1234-1234-123456789ABC"


def service_receipt():
    text = guest_services.POLICY.read_text()
    return {"policy_sha256": hashlib.sha256(text.encode()).hexdigest(),
            "provision_boot_session": DATA_UUID, "verified_boot_session": NEXT_BOOT,
            "results": {label: {"outcome": "kept-sip" if expected == "kept-sip" else "off",
                                "disabled": expected != "kept-sip", "pid": 0}
                        for label, expected in guest_services.parse(text).items()}}


def pending():
    return pinlock.seed(SERVER)


def ready(rebuild=None):
    if rebuild is None:
        rebuild = ("1.100.0-x86_64-unknown-linux-gnu" in SERVER["rust_toolchains"]
                   and "1.100.0" not in parity.PENDING_MACOS_TOOLCHAINS)
    manifest = pending()
    if not rebuild:
        # Keep the existing parity tests exercising the measured live-image shape.
        manifest["rust_default"] = "1.99.0-aarch64-apple-darwin"
        manifest["rust_toolchains"] = {name: manifest["rust_toolchains"][name]
                                       for name in parity.rust_toolchains(SERVER) if name != parity.REBUILD_RUST}
        del manifest["sources"]["cmake"]
        del manifest["tools"]["cmake"]

    def fill(value) -> Any:
        if isinstance(value, dict):
            return {key: fill(item) for key, item in value.items()}
        if isinstance(value, list):
            return [fill(item) for item in value]
        return "a" * 64 if value == pinlock.PENDING else value

    manifest = fill(manifest)
    manifest["release_status"] = "reviewed"
    manifest["clt"]["version"] = "27.0.0.0.1.1790000000"
    manifest["tools"]["bash"]["version"] = "GNU bash, version 5.2.37(1)-release (aarch64-apple-darwin26.0.0)"
    for row in manifest["rust_toolchains"].values():
        row["rustc_verbose"] = row["rustc"] + "\nbinary: rustc\nhost: aarch64-apple-darwin"
    return manifest


def observed(manifest, tmp):
    result = {key: copy.deepcopy(manifest[key]) for key in ("rustup", "rust_default", "rust_profile", "rust_toolchains")}
    result.update(stable_copied=True, rustup_writable=False, clt=manifest["clt"]["version"],
                  installed_manifest_sha256=parity.manifest_digest(manifest),
                  rust_target_payloads={name: True for name in manifest["rust_toolchains"]},
                  tools=copy.deepcopy(manifest["tools"]), cargo_binaries={},
                  job_environment={key: value for key, value in parity.ENVIRONMENT.items() if key != "TMPDIR"})
    result["job_environment"]["TMPDIR"] = str(tmp)
    for name, spec in manifest["cargo_binaries"].items():
        result["cargo_binaries"][name] = ({"symlink": "rustup"} if spec.get("proxy") else copy.deepcopy(spec))
    return result


class ParityTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="motor-job-")
        self.addCleanup(self.scratch.cleanup)
        self.manifest = ready()
        self.actual = observed(self.manifest, Path(self.scratch.name))

    def test_pending_lock_cannot_be_deployed_or_checked(self):
        manifest = pending()
        self.assertEqual(manifest["ipsw"]["build"], "26B123")
        self.assertEqual(manifest["ipsw"]["size"], 25595669503)
        self.assertEqual(manifest["ipsw"]["sha256"], "6613b34e8589db2202a01ec37a2648afb141ece13428175183f8f3e38438534f")
        with self.assertRaisesRegex(ValueError, "not reviewed"):
            pinlock.require_ready(manifest)
        with self.assertRaisesRegex(ValueError, "pending"):
            parity.validate(manifest, SERVER)

    def test_complete_fixture_has_exact_mac_only_pins(self):
        pinlock.require_ready(self.manifest)
        parity.validate(self.manifest, SERVER)
        self.assertEqual(parity.differences(self.manifest, self.actual, job=True), [])
        self.assertEqual(self.manifest["rust_default"], "1.99.0-aarch64-apple-darwin")
        for chain in self.manifest["rust_toolchains"].values():
            self.assertEqual(chain["targets"], ["aarch64-apple-darwin"])
        self.assertEqual(self.manifest["sources"]["bash"]["version"], "5.2.37")

    def test_real_manifest_validates_against_real_server_manifest(self):
        manifest = json.loads((HERE / "toolchain.json").read_text())
        parity.validate(manifest, SERVER)

    def test_live_macos_default_remains_valid_after_linux_default_promotion(self):
        manifest = json.loads((HERE / "toolchain.json").read_text())
        self.assertEqual(SERVER["rust_default"], "1.100.0-x86_64-unknown-linux-gnu")
        self.assertEqual(manifest["rust_default"], "1.99.0-aarch64-apple-darwin")
        parity.validate(manifest, SERVER)

    def test_default_not_installed_on_server_is_rejected(self):
        self.manifest["rust_default"] = "1.98.0-aarch64-apple-darwin"
        with self.assertRaisesRegex(ValueError, "not a shared installed toolchain"):
            parity.validate(self.manifest, SERVER)

    def test_linux_default_not_installed_in_macos_image_is_rejected(self):
        self.manifest["rust_default"] = "1.100.0-aarch64-apple-darwin"
        with self.assertRaisesRegex(ValueError, "not a shared installed toolchain"):
            parity.validate(self.manifest, SERVER)

    def test_observed_default_must_still_match_sealed_macos_default(self):
        self.actual["rust_default"] = "1.100.0-aarch64-apple-darwin"
        errors = parity.differences(self.manifest, self.actual, job=True)
        self.assertEqual(len(errors), 1)
        self.assertIn("rust_default:", errors[0])

    def test_linux_rust_1_100_waits_for_the_next_macos_image(self):
        manifest = json.loads((HERE / "toolchain.json").read_text())
        self.assertIn("1.100.0-x86_64-unknown-linux-gnu", SERVER["rust_toolchains"])
        self.assertNotIn("1.100.0-" + parity.HOST, manifest["rust_toolchains"])
        self.assertIn("1.100.0", parity.PENDING_MACOS_TOOLCHAINS)
        parity.validate(manifest, SERVER)
        with patch.object(parity, "PENDING_MACOS_TOOLCHAINS", parity.PENDING_MACOS_TOOLCHAINS - {"1.100.0"}):
            with self.assertRaisesRegex(ValueError, "differ from build-server"):
                parity.validate(manifest, SERVER)

    def test_bun_version_outside_server_and_pending_versions_is_rejected(self):
        self.manifest["tools"]["bun"]["version"] = "1.4.3"
        with self.assertRaisesRegex(ValueError, "macOS tool version differs from build-server: bun"):
            parity.validate(self.manifest, SERVER)

    def test_server_bun_1_4_4_is_accepted(self):
        self.manifest["tools"]["bun"]["version"] = "1.4.4"
        parity.validate(self.manifest, SERVER)

    def test_build_server_rustc_drift_is_rejected_before_capture(self):
        server = copy.deepcopy(SERVER)
        server["rust_toolchains"]["1.99.0-x86_64-unknown-linux-gnu"]["rustc"] = "rustc 1.99.0 (deadbeef 2026-09-28)"
        with self.assertRaisesRegex(ValueError, "differ from build-server"):
            parity.validate(self.manifest, server)

    def test_pending_component_is_skipped_but_any_other_new_component_is_not(self):
        server = copy.deepcopy(SERVER)
        server["rust_toolchains"].pop("1.100.0-x86_64-unknown-linux-gnu", None)
        manifest = ready(rebuild=False)
        components = server["rust_toolchains"]["1.99.0-x86_64-unknown-linux-gnu"]["components"]
        components.append("miri-x86_64-unknown-linux-gnu")
        with patch.object(parity, "PENDING_MACOS_COMPONENTS", {"miri"}):
            parity.validate(manifest, server)
        with self.assertRaisesRegex(ValueError, "differ from build-server"):
            parity.validate(manifest, server)

    def test_pending_toolchain_is_skipped_but_any_other_new_toolchain_is_not(self):
        server = copy.deepcopy(SERVER)
        server["rust_toolchains"].pop("1.100.0-x86_64-unknown-linux-gnu", None)
        manifest = ready(rebuild=False)
        chains = server["rust_toolchains"]
        base = chains["1.99.0-x86_64-unknown-linux-gnu"]
        chains["1.93.0-x86_64-unknown-linux-gnu"] = dict(base, rustc="rustc 1.93.0 (aaaaaaaaa 2026-01-22)")
        with patch.object(parity, "PENDING_MACOS_TOOLCHAINS", parity.PENDING_MACOS_TOOLCHAINS | {"1.93.0"}):
            parity.validate(manifest, server)
        with self.assertRaisesRegex(ValueError, "differ from build-server"):
            parity.validate(manifest, server)

    def test_macos_pin_drift_compares_with_sealed_manifest(self):
        self.manifest["sources"]["bash"]["sha256"] = "b" * 64
        errors = parity.differences(self.manifest, self.actual, job=True)
        self.assertEqual(len(errors), 1)
        self.assertIn("sealed pin lock", errors[0])

    def test_stable_must_be_an_independent_copy_and_have_payloads(self):
        self.actual["stable_copied"] = False
        self.assertIn("stable copied", " ".join(parity.differences(self.manifest, self.actual)))
        self.actual["stable_copied"] = True
        self.actual["rust_target_payloads"]["stable-aarch64-apple-darwin"] = False
        self.assertIn("libstd payload", " ".join(parity.differences(self.manifest, self.actual)))

    def test_extra_cross_target_and_missing_components_are_rejected(self):
        for change in (lambda row: row["targets"].append("wasm32-unknown-unknown"),
                       lambda row: row["components"].pop()):
            manifest = copy.deepcopy(self.manifest)
            change(manifest["rust_toolchains"][manifest["rust_default"]])
            with self.assertRaisesRegex(ValueError, "toolchains/components"):
                parity.validate(manifest, SERVER)

    def test_hash_versions_clt_and_environment_drift_fail(self):
        for path, value in ((["tools", "bun", "sha256"], "b" * 64),
                            (["tools", "bash", "version"], "GNU bash, version 3.2"),
                            (["cargo_binaries", "cargo-nextest", "sha256"], "b" * 64),
                            (["clt"], "old"), (["job_environment", "PATH"], "/bin"),
                            (["job_environment", "CARGO_TARGET_DIR"], "/poison")):
            with self.subTest(path=path):
                actual = copy.deepcopy(self.actual)
                parent = actual
                for key in path[:-1]:
                    parent = parent[key]
                parent[path[-1]] = value
                self.assertTrue(parity.differences(self.manifest, actual, job=True))

    def test_job_rejects_writable_rustup_even_without_linux_mountinfo(self):
        self.actual["rustup_writable"] = True
        errors = parity.differences(self.manifest, self.actual, job=True)
        self.assertEqual(len(errors), 1)
        self.assertIn("job rustup home writable", errors[0])
        self.assertEqual(parity.differences(self.manifest, self.actual, job=False), [])

    def test_dispatcher_loads_build_server_manifest_for_macos(self):
        spec = importlib.util.spec_from_file_location("parity_dispatch", HERE.parent / "runner/toolchain-parity.py")
        if spec is None or spec.loader is None:
            raise ImportError("cannot load parity dispatcher")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with patch.object(module, "capture", return_value=self.actual) as capture:
            result = module.check(self.manifest, "macos", True)
        self.assertTrue(result["passed"], result)
        capture.assert_called_once_with(self.manifest, job=True, role="macos")
        with patch.object(module, "MANIFEST", HERE / "toolchain.json"), patch.object(module, "capture") as capture:
            with self.assertRaises((KeyError, ValueError)):
                module.check(self.manifest, "macos", True)
            capture.assert_not_called()

    def test_capture_never_reads_linux_inventory(self):
        root = Path(self.scratch.name)
        cargo, rustup = root / "cargo", root / "rustup"
        (cargo / "bin").mkdir(parents=True)
        (cargo / "bin/rustup").write_bytes(b"rustup")
        (cargo / "bin/rustup").chmod(0o755)
        for name in ("1.99.0-aarch64-apple-darwin", "stable-aarch64-apple-darwin"):
            chain = rustup / "toolchains" / name
            (chain / "bin").mkdir(parents=True)
            for binary in ("cargo", "rustc"):
                (chain / "bin" / binary).write_bytes(binary.encode())
            lib = chain / "lib/rustlib/aarch64-apple-darwin/lib"
            lib.mkdir(parents=True)
            (lib / "libstd-abc.rlib").write_bytes(b"std")
        manifest = dict(self.manifest, cargo_home=str(cargo), rustup_home=str(rustup))
        (root / "toolchain.json").write_text(json.dumps(manifest))
        commands = []

        def output(*args, **kwargs):
            commands.append(args)
            if args[0] == "/usr/sbin/pkgutil":
                return plistlib.dumps({"pkg-version": "27.0"}).decode()
            if args[1:] == ("toolchain", "list"):
                return "1.99.0-aarch64-apple-darwin\nstable-aarch64-apple-darwin"
            if args[1:3] == ("target", "list"):
                return "aarch64-apple-darwin"
            if args[1:3] == ("component", "list"):
                return "rustc-aarch64-apple-darwin"
            if args[1:] == ("default",):
                return "1.99.0-aarch64-apple-darwin (default)"
            if args[1:] == ("show", "profile"):
                return "minimal"
            return "rustup 1.29.1" if str(args[0]).endswith("rustup") else "rustc 1.99.0"

        with patch.object(parity, "PREFIX", str(root)), patch.object(parity, "output", side_effect=output), patch.object(parity.shutil, "which", return_value=None):
            actual = parity.capture(manifest, job=True)
        self.assertTrue(actual["stable_copied"])
        self.assertNotIn("mountinfo", actual)
        self.assertNotIn("apt_packages", actual)
        self.assertFalse(any("dpkg" in str(command) or "/srv/build" in str(command) for command in commands))


class RebuildRecipeTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="macos-rebuild-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)

    def test_seed_includes_rust_1_100_with_exact_versions_and_server_default(self):
        with patch.object(parity, "PENDING_MACOS_TOOLCHAINS", {"1.92.0", "1.100.0"}), \
                patch.object(parity, "PENDING_MACOS_COMPONENTS", {"llvm-tools", "rust-analyzer"}):
            manifest = pending()
        chains = manifest["rust_toolchains"]
        new = chains["1.100.0-aarch64-apple-darwin"]
        old = chains["1.99.0-aarch64-apple-darwin"]
        self.assertEqual(manifest["rust_default"], "1.100.0-aarch64-apple-darwin")
        self.assertEqual(manifest["stable_copy_from"], "1.99.0-aarch64-apple-darwin")
        self.assertEqual(new["rustc"], "rustc 1.100.0 (0ef1bc386 2026-11-09)")
        self.assertEqual(new["cargo"], "cargo 1.100.0 (3acc79c0c 2026-11-05)")
        self.assertEqual(new["targets"], ["aarch64-apple-darwin"])
        self.assertEqual(new["components"], old["components"])
        self.assertIn("llvm-tools-aarch64-apple-darwin", new["components"])
        self.assertIn("rust-analyzer-aarch64-apple-darwin", new["components"])
        self.assertIn("1.92.0-aarch64-apple-darwin", chains)
        self.assertEqual(new["sha256"], pinlock.PENDING)

    def test_seed_preserves_an_older_explicit_server_default(self):
        server = copy.deepcopy(SERVER)
        server["rust_default"] = "1.99.0-x86_64-unknown-linux-gnu"
        self.assertEqual(pinlock.seed(server)["rust_default"], "1.99.0-aarch64-apple-darwin")

    def test_seed_pins_official_universal_cmake_at_linux_version(self):
        manifest = pending()
        self.assertEqual(manifest["sources"]["cmake"], {
            "version": "4.2.3",
            "url": "https://github.com/Kitware/CMake/releases/download/v4.2.3/cmake-4.2.3-macos-universal.tar.gz",
            "sha256": "c2302d3e9c48daabee5ea7c4db4b2b93b989bcc89dae8b760880e00120641b5b",
        })
        self.assertEqual(manifest["tools"]["cmake"]["version"], "cmake version 4.2.3")
        self.assertEqual(manifest["tools"]["cmake"]["version"], SERVER["tools"]["cmake"]["observed_build_server_version"])
        self.assertEqual(manifest["tools"]["cmake"]["path"], "/opt/motor/bin/cmake")

    def test_rebuilt_manifest_validates_before_and_after_linux_catches_up(self):
        manifest = ready(rebuild=True)
        pinlock.require_ready(manifest)
        parity.validate(manifest, SERVER)
        server = copy.deepcopy(SERVER)
        server["rust_toolchains"]["1.100.0-x86_64-unknown-linux-gnu"] = dict(
            server["rust_toolchains"]["1.99.0-x86_64-unknown-linux-gnu"],
            rustc="rustc 1.100.0 (0ef1bc386 2026-11-09)")
        with patch.object(parity, "PENDING_MACOS_TOOLCHAINS", {"1.100.0"}):
            parity.validate(manifest, server)
            legacy = ready(rebuild=False)
            parity.validate(legacy, server)
        with patch.object(parity, "PENDING_MACOS_TOOLCHAINS", set()):
            parity.validate(manifest, server)
            with self.assertRaisesRegex(ValueError, "toolchains/components"):
                parity.validate(legacy, server)

    def test_rebuilt_manifest_rejects_cmake_pin_and_rust_identity_drift(self):
        for path, value in ((["tools", "cmake", "version"], "cmake version 4.2.4"),
                            (["tools", "cmake", "path"], "/usr/bin/cmake"),
                            (["sources", "cmake", "sha256"], "b" * 64),
                            (["rust_toolchains", parity.REBUILD_RUST, "cargo"], "cargo 1.99.0"),
                            (["rust_toolchains", parity.REBUILD_RUST, "rustc"], "rustc 1.100.0 (wrong 2026-11-09)")):
            with self.subTest(path=path):
                manifest = ready(rebuild=True)
                parent = manifest
                for key in path[:-1]:
                    parent = parent[key]
                parent[path[-1]] = value
                with self.assertRaises(ValueError):
                    parity.validate(manifest, SERVER)
        manifest = ready(rebuild=True)
        del manifest["tools"]["cmake"]
        with self.assertRaisesRegex(ValueError, "tool set"):
            parity.validate(manifest, SERVER)

    def test_measurement_keeps_exact_requested_identities_and_records_hashes(self):
        manifest = ready(rebuild=True)
        actual = observed(manifest, self.root)
        actual["tools"]["cmake"]["sha256"] = "b" * 64
        actual["rust_toolchains"][parity.REBUILD_RUST]["sha256"] = "c" * 64
        candidate = pinlock.candidate(manifest, actual, {}, "d" * 64, {})
        self.assertEqual(candidate["tools"]["cmake"]["sha256"], "b" * 64)
        self.assertEqual(candidate["rust_toolchains"][parity.REBUILD_RUST]["sha256"], "c" * 64)
        self.assertEqual(candidate["rust_toolchains"][parity.REBUILD_RUST]["cargo"], "cargo 1.100.0 (3acc79c0c 2026-11-05)")
        for key, value in (("cargo", "cargo 1.99.0"), ("rustc", "rustc 1.99.0"),
                           ("components", []), ("targets", [])):
            with self.subTest(key=key):
                wrong = copy.deepcopy(actual)
                wrong["rust_toolchains"][parity.REBUILD_RUST][key] = value
                with self.assertRaisesRegex(ValueError, "measured Rust " + key):
                    pinlock.candidate(manifest, wrong, {}, "d" * 64, {})
        wrong = copy.deepcopy(actual)
        wrong["tools"]["cmake"]["version"] = "cmake version 4.2.4"
        with self.assertRaisesRegex(ValueError, "measured tool version differs: cmake"):
            pinlock.candidate(manifest, wrong, {}, "d" * 64, {})
        wrong = copy.deepcopy(actual)
        del wrong["rust_toolchains"][parity.REBUILD_RUST]
        with self.assertRaisesRegex(ValueError, "toolchain names"):
            pinlock.candidate(manifest, wrong, {}, "d" * 64, {})
        self.assertTrue(parity.differences(manifest, actual))

    def test_capture_measures_numbered_cargo_and_cmake_version_and_hash(self):
        manifest = ready(rebuild=True)
        cargo, rustup = self.root / "cargo", self.root / "rustup"
        (cargo / "bin").mkdir(parents=True)
        (cargo / "bin/rustup").write_bytes(b"rustup")
        (cargo / "bin/rustup").chmod(0o755)
        for name in manifest["rust_toolchains"]:
            chain = rustup / "toolchains" / name
            (chain / "bin").mkdir(parents=True)
            for binary in ("cargo", "rustc"):
                (chain / "bin" / binary).write_bytes(binary.encode())
            lib = chain / "lib/rustlib/aarch64-apple-darwin/lib"
            lib.mkdir(parents=True)
            (lib / "libstd-test.rlib").write_bytes(b"std")
        cmake = self.root / "cmake"
        cmake.write_bytes(b"official cmake executable")
        manifest.update(cargo_home=str(cargo), rustup_home=str(rustup))
        (self.root / "toolchain.json").write_text(json.dumps(manifest))

        def output(*args, **kwargs):
            if args[0] == "/usr/sbin/pkgutil":
                return plistlib.dumps({"pkg-version": "27.0"}).decode()
            if args[1:] == ("toolchain", "list"):
                return "\n".join(manifest["rust_toolchains"])
            if args[1:3] == ("target", "list"):
                return "aarch64-apple-darwin"
            if args[1:3] == ("component", "list"):
                return "rustc-aarch64-apple-darwin"
            if args[1:] == ("default",):
                return "1.99.0-aarch64-apple-darwin (default)"
            if args[1:] == ("show", "profile"):
                return "minimal"
            if args[0] == str(cmake):
                return "cmake version 4.2.3\n\nCMake suite maintained and supported by Kitware"
            if args[0] == str(rustup / "toolchains/1.100.0-aarch64-apple-darwin/bin/cargo"):
                return "cargo 1.100.0 (3acc79c0c 2026-11-05)"
            return "rustup 1.29.1" if args[0].endswith("rustup") else "rustc 1.99.0"

        with patch.object(parity, "PREFIX", str(self.root)), patch.object(parity, "output", side_effect=output) as command, \
                patch.object(parity.shutil, "which", side_effect=lambda name, **kw: str(cmake) if name == "cmake" else None):
            actual = parity.capture(manifest)
        command.assert_any_call(str(rustup / "toolchains/1.100.0-aarch64-apple-darwin/bin/cargo"), "--version", env=ANY)
        self.assertEqual(actual["rust_toolchains"][parity.REBUILD_RUST]["cargo"], "cargo 1.100.0 (3acc79c0c 2026-11-05)")
        self.assertEqual(actual["tools"]["cmake"]["version"], "cmake version 4.2.3")
        self.assertEqual(actual["tools"]["cmake"]["sha256"], hashlib.sha256(b"official cmake executable").hexdigest())

    def test_cmake_install_preserves_modules_and_exposes_only_cli_on_job_path(self):
        archive = self.root / "cmake.tar.gz"
        contents = "cmake-4.2.3-macos-universal/CMake.app/Contents/"
        files = {"bin/cmake": b"cmake", "bin/ctest": b"ctest", "bin/cpack": b"cpack",
                 "share/cmake-4.2/Modules/CMake.cmake": b"module", "MacOS/CMake": b"gui"}
        with tarfile.open(archive, "w:gz") as output:
            for name, data in files.items():
                info = tarfile.TarInfo(contents + name)
                info.mode, info.size = (0o755 if name.startswith("bin/") else 0o644), len(data)
                output.addfile(info, io.BytesIO(data))
            info = tarfile.TarInfo(contents + "bin/cmake-gui")
            info.type, info.linkname = tarfile.SYMTYPE, "../MacOS/CMake"
            output.addfile(info)
        prefix = self.root / "opt/motor"
        (prefix / "bin").mkdir(parents=True)
        with patch.object(guest, "PREFIX", prefix):
            guest.install_cmake(archive, self.root)
        for name in ("cmake", "ctest", "cpack"):
            path = prefix / "bin" / name
            self.assertEqual(os.readlink(path), "../cmake/bin/" + name)
            self.assertEqual(path.read_bytes(), name.encode())
            self.assertTrue(os.access(path, os.X_OK))
        self.assertEqual((prefix / "cmake/share/cmake-4.2/Modules/CMake.cmake").read_bytes(), b"module")
        self.assertFalse((prefix / "cmake/bin/cmake-gui").exists())
        self.assertFalse((prefix / "cmake/MacOS").exists())
        with patch.object(guest, "PREFIX", prefix), patch.object(guest.os, "chown") as chown:
            guest.make_readonly()
        chown.assert_any_call(prefix / "cmake/bin/cmake", 0, 0)
        self.assertEqual((prefix / "bin/cmake").stat().st_mode & 0o777, 0o555)
        self.assertEqual((prefix / "cmake/share/cmake-4.2/Modules/CMake.cmake").stat().st_mode & 0o777, 0o444)
        for path in [prefix, *prefix.rglob("*")]:
            if not path.is_symlink():
                path.chmod(0o755 if path.is_dir() else 0o644)

    def test_selected_cmake_archive_members_cannot_escape_scratch(self):
        archive = self.root / "unsafe.tar.gz"
        with tarfile.open(archive, "w:gz") as output:
            info = tarfile.TarInfo("contents/share/escape")
            info.type, info.linkname = tarfile.SYMTYPE, "../../outside"
            output.addfile(info)
        with self.assertRaisesRegex(ValueError, "unsafe source archive link"):
            guest.unpack(archive, self.root / "unpack", include=["contents/share"])


class InstallerTests(unittest.TestCase):
    def test_metrics_default_off_and_opt_in_arguments(self):
        args = (Path('/runner'), Path('/state'), 'macos', Path('/vm.toml'))
        plain = install.agent(*args)['ProgramArguments']
        self.assertEqual(plain, ['/runner', 'serve', '--instance', 'macos', '--state-root', '/state',
                                 '--backend', 'macos-vm', '--vm-config', '/vm.toml', '--max-weight', '8',
                                 '--return-chunked-max-bytes', '134217728',
                                 '--return-budget-bytes', '536870912'])
        self.assertFalse(any(arg.startswith('--metrics-') for arg in plain))
        enabled = install.agent(*args, 'https://metrics.example:8443', 'monitor-1')['ProgramArguments']
        self.assertEqual(enabled, plain + ['--metrics-push', '--metrics-endpoint',
                                         'https://metrics.example:8443', '--metrics-host-id', 'monitor-1'])
        self.assertEqual(install.metrics_settings(enabled), ['https://metrics.example:8443', 'monitor-1'])

    def test_metrics_refuse_half_or_invalid_configuration(self):
        pairs = [('https://metrics.example', None), (None, 'monitor-1')]
        pairs += [(endpoint, 'monitor-1') for endpoint in (
            'http://metrics.example', 'https://metrics.example/path', 'https://user@metrics.example',
            'https://metrics.example?query', 'https://metrics.example#fragment',
            'https://metrics.example:99999', 'https://metrics.example\\evil', 'https://metrics.example/\n')]
        pairs += [('https://metrics.example', host_id) for host_id in ('bad id', 'bad/id', 'x' * 65)]
        for endpoint, host_id in pairs:
            with self.subTest(endpoint=endpoint, host_id=host_id), self.assertRaises(ValueError):
                install.agent(Path('/runner'), Path('/state'), 'macos', Path('/vm.toml'), endpoint, host_id)

    def test_metrics_stage_key_and_check_report_use_installed_arguments_without_start(self):
        output = io.StringIO()
        with patch.object(install, 'PLIST', self.plist), patch.object(install.os, 'chown'), \
                patch.object(install.os, 'fchown'), patch('sys.stdout', output), \
                patch.object(install, 'run', return_value='a' * 64 + '\n') as run:
            before = plistlib.loads(self.plist.read_bytes())
            install.stage_metrics('https://metrics.example', 'monitor-1')
            self.assertFalse(run.called)
            staged = plistlib.loads(self.plist.read_bytes())
            flags = ['--metrics-push', '--metrics-endpoint', 'https://metrics.example', '--metrics-host-id', 'monitor-1']
            self.assertEqual(staged['ProgramArguments'], before['ProgramArguments'] + flags)
            self.assertEqual(json.loads(output.getvalue())['metrics']['flags'], flags)
            with patch.object(install, 'run', side_effect=self.command):
                self.assertEqual(self.check(), [])
            output.truncate(0)
            output.seek(0)
            install.installed_metrics_key()
            self.assertEqual(output.getvalue().strip(), 'a' * 64)
            run.assert_called_once_with(*install.as_motor(str(self.config / 'motor-runner'),
                                        'metrics-key', '--instance', 'macos', '--state-root', str(self.state)))
            run.reset_mock()
            with patch.object(install.sys, 'platform', 'darwin'), patch.object(install.os, 'geteuid', return_value=0), \
                    patch.object(install.sys, 'argv', ['install.py', '--check', '--image', str(self.image),
                                                     '--state-root', str(self.state)]), \
                    patch.object(install, 'CONFIG', self.config), patch.object(install, 'check_install', return_value=[]):
                output.truncate(0)
                output.seek(0)
                self.assertEqual(install.main(), 0)
                self.assertEqual(json.loads(output.getvalue())['metrics']['flags'], flags)
            install.stage_metrics(None, None)
            self.assertEqual(plistlib.loads(self.plist.read_bytes()), before)
            self.assertFalse(run.called)

    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.state, self.image, self.config = [self.root / name for name in ("state", "image", "config")]
        for path in (self.state, self.image, self.config):
            path.mkdir()
        self.manifest = ready()
        (self.config / "toolchain.json").write_text(json.dumps(self.manifest))
        self.ssh = self.root / "ssh"
        self.ssh.mkdir()
        (self.ssh / "authorized_keys").write_text("ssh-ed25519 AAAA\n")
        self.sshd = self.root / "sshd_config"
        self.sshd.write_text("Port 22\n" + install.ssh_block(self.ssh / "authorized_keys"))
        installed = {"manifest_sha256": parity.digest(self.config / "toolchain.json"),
                     "operator_account": "operator",
                     "helper": str(self.config / "motor-macvm"), "runner": str(self.config / "motor-runner"),
                     "authorized_keys_sha256": parity.digest(self.ssh / "authorized_keys")}
        (self.config / "install.json").write_text(json.dumps(installed))
        (self.config / "vm.toml").write_text(install.vm_configuration(self.image, Path(installed["helper"]), self.state / "tools", self.manifest))
        self.plist = self.root / "agent.plist"
        self.plist.write_bytes(plistlib.dumps(install.agent(Path(installed["runner"]), self.state, "macos", self.config / "vm.toml")))
        files = {}
        for name in (*image.IMAGE_FILES, "agent.sha256"):
            (self.image / name).write_bytes(name.encode())
            files[name] = parity.digest(self.image / name)
        self.requirement = 'cdhash H"0123456789abcdef0123456789abcdef01234567"'
        (self.image / "image.json").write_text(json.dumps({"manifest_sha256": installed["manifest_sha256"], "files": files,
            "tcc": {"services": ["kTCCServiceSystemPolicyNetworkVolumes", "kTCCServiceSystemPolicyAllFiles"],
                    "requirement": self.requirement}}))
        for path in self.image.iterdir():
            path.chmod(0o444)
        self.image.chmod(0o555)
        (self.state / "macos").mkdir()
        self.socket = socket.socket(socket.AF_UNIX)
        # AF_UNIX paths are short even on macOS's long temporary-directory names.
        self.socket_path = self.state / "macos/runner.sock"
        try:
            self.socket.bind(str(self.socket_path))
        except OSError:
            self.socket.close()
            self.skipTest("temporary path exceeds this platform's AF_UNIX path limit")
        self.addCleanup(self.socket.close)
        self.socket_path.chmod(0o600)
        self.connect = self.root / "home/.cargo/bin/motor-runner"
        self.connect.parent.mkdir(parents=True)
        self.connect.symlink_to(self.config / "motor-runner")
        self.patches = [patch.object(install, "internal_volume", return_value=True),
                        patch.object(install, "entitlement", return_value=True),
                        patch.object(install, "tool_errors", return_value=[]),
                        patch.object(install.shutil, "disk_usage", return_value=shutil._ntuple_diskusage(100, 50, install.FLOOR)),
                        patch.object(install.pwd, "getpwnam", return_value=type("Account", (), {"pw_uid": os.getuid()})),
                         patch.object(install, "LEGACY_PLIST", self.root / "legacy.plist"),
                         patch.object(image, "mounted_data", return_value=nullcontext(self.image)),
                         patch.object(image, "agent_requirement", return_value=self.requirement),
                         patch.object(install.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "no-timeout")),
                         patch.object(install, "run", side_effect=self.command),
                         patch.dict(install.os.environ, {"SUDO_USER": "operator"}),
                         patch.object(install, "HOME", self.root / "home")]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)

    def command(self, *args):
        if args[0] == "/usr/bin/fdesetup":
            return "FileVault is Off."
        if args[0] == "/usr/bin/defaults":
            return plistlib.dumps({"autoLoginUser": "motor"}).decode()
        if args[0] == "/bin/launchctl":
            return "state = running"
        return f"trustedusercakeys none\nauthorizedkeysfile {self.ssh}/authorized_keys"

    def check(self):
        return install.check_install(self.state, self.image, "macos", self.manifest, self.config, self.plist, self.sshd, self.ssh)

    def test_valid_installed_fixture_passes(self):
        self.assertEqual(self.check(), [])

    def test_check_requires_the_connect_link_to_the_installed_runner(self):
        # ck-motor's SSH session runs ~/.cargo/bin/motor-runner as motor.
        self.connect.unlink()
        self.assertIn("connect path", " ".join(self.check()))
        self.connect.symlink_to(self.root / "elsewhere/motor-runner")
        self.assertIn("connect path", " ".join(self.check()))

    def test_no_aqua_session_reports_restart_even_without_socket(self):
        self.socket_path.unlink()
        with patch.object(install, "has_gui_session", return_value=False):
            errors = " ".join(self.check())
        self.assertIn("no Aqua login session", errors)
        self.assertIn("restart for auto-login", errors)
        self.assertIn("runner socket is missing", errors)

    def test_wrong_auto_login_user_and_filevault_are_refused(self):
        with patch.object(install, "auto_login_user", return_value="operator"):
            self.assertIn("autoLoginUser must be motor", " ".join(self.check()))
        with patch.object(install, "run", side_effect=lambda *args: "FileVault is On." if args[0] == "/usr/bin/fdesetup" else self.command(*args)):
            self.assertIn("FileVault must be off", " ".join(self.check()))

    def test_keychain_timeout_or_failed_probe_is_refused(self):
        for result in (subprocess.CompletedProcess([], 0, "", "timeout=300s"),
                       subprocess.CompletedProcess([], 1, "", "no-timeout")):
            with self.subTest(result=result), patch.object(install.subprocess, "run", return_value=result):
                self.assertIn("keychain must have no-timeout", " ".join(self.check()))

    def test_changed_agent_requirement_is_refused(self):
        with patch.object(image, "agent_requirement", return_value='cdhash H"different"'):
            self.assertIn("requirement differs from sealed TCC grant", " ".join(self.check()))

    def test_incomplete_tcc_record_is_refused(self):
        path = self.image / "image.json"
        pin = json.loads(path.read_text())
        for grant in (None, {}, {"services": ["kTCCServiceSystemPolicyAllFiles"], "requirement": self.requirement},
                      {"services": list(image.TCC_SERVICES), "requirement": ""}):
            path.chmod(0o644)
            path.write_text(json.dumps(dict(pin, tcc=grant)))
            path.chmod(0o444)
            with self.subTest(grant=grant), patch.object(image, "mounted_data") as mount:
                self.assertIn("missing the guest agent TCC grant record", " ".join(self.check()))
                mount.assert_not_called()

    def test_agent_must_be_running_in_gui_domain(self):
        target = f"gui/{os.getuid()}/{install.LABEL}"
        for failure in ("state = waiting", subprocess.CalledProcessError(113, "launchctl")):
            def command(*args):
                if args == ("/bin/launchctl", "print", target):
                    if isinstance(failure, Exception):
                        raise failure
                    return failure
                return self.command(*args)
            with self.subTest(failure=failure), patch.object(install, "run", side_effect=command):
                self.assertIn("LaunchAgent is not", " ".join(self.check()))

    def test_agent_requires_aqua_without_daemon_identity_fields(self):
        valid = plistlib.loads(self.plist.read_bytes())
        self.assertEqual(valid.get("LimitLoadToSessionType"), "Aqua")
        self.assertNotIn("UserName", valid)
        self.assertNotIn("GroupName", valid)
        for changes in ({"LimitLoadToSessionType": "Background"}, {"UserName": "motor"}, {"GroupName": "staff"}):
            self.assertIn("Aqua session", " ".join(install.plist_errors({**valid, **changes})))

    def test_legacy_daemon_is_refused(self):
        install.LEGACY_PLIST.touch()
        self.assertIn("legacy LaunchDaemon", " ".join(self.check()))

    def test_external_state_and_external_image_are_each_refused(self):
        for external in (self.state, self.image):
            with self.subTest(path=external), patch.object(install, "internal_volume", side_effect=lambda path: path != external):
                self.assertIn("not on an internal", " ".join(self.check()))

    def test_missing_virtualization_entitlement_is_refused(self):
        with patch.object(install, "entitlement", return_value=False):
            self.assertIn("lacks virtualization entitlement", " ".join(self.check()))

    def test_unpinned_or_ca_trusting_effective_sshd_is_refused(self):
        cases = {"authorizedkeysfile .ssh/authorized_keys": "does not pin the runner key file",
                 f"trustedusercakeys /etc/ca.pub\nauthorizedkeysfile {self.ssh}/authorized_keys": "trusts a user CA"}
        # The sshd on macOS 27.2 prints keywords in mixed case, older ones in
        # lower case; both must be read.
        cases[f"TrustedUserCAKeys /etc/ca.pub\nAuthorizedKeysFile {self.ssh}/authorized_keys"] = "trusts a user CA"
        cases["AuthorizedKeysFile .ssh/authorized_keys"] = "does not pin the runner key file"
        for effective, expected in cases.items():
            with self.subTest(effective=effective), patch.object(install, "run", side_effect=lambda *args: effective if args[:2] == ("/usr/sbin/sshd", "-T") else self.command(*args)):
                self.assertIn(expected, " ".join(self.check()))

    def test_mixed_case_pinned_effective_sshd_passes(self):
        effective = f"TrustedUserCAKeys none\nAuthorizedKeysFile {self.ssh}/authorized_keys"
        with patch.object(install, "run", side_effect=lambda *args: effective if args[:2] == ("/usr/sbin/sshd", "-T") else self.command(*args)):
            self.assertFalse([error for error in self.check() if "sshd" in error])

    def test_changed_pinned_runner_key_is_refused(self):
        (self.ssh / "authorized_keys").write_text("ssh-ed25519 BBBB\n")
        self.assertIn("pinned runner key changed", self.check())

    def test_world_readable_runner_socket_is_refused(self):
        self.socket_path.chmod(0o666)
        self.assertIn("socket must be", " ".join(self.check()))

    def test_changed_pinned_tool_bytes_are_refused(self):
        tools = self.root / "tools"
        (tools / "bin").mkdir(parents=True)
        for name, spec in self.manifest["prep_tools"].items():
            (tools / spec["path"]).write_bytes(name.encode())
            spec["sha256"] = parity.digest(tools / spec["path"])
        real = self.patches[2]
        real.stop()
        self.assertEqual(install.tool_errors(tools, self.manifest), [])
        (tools / "bin/cargo").write_bytes(b"poison")
        self.assertEqual(install.tool_errors(tools, self.manifest), ["pinned tool SHA-256 differs: cargo"])
        real.start()
        with patch.object(install, "tool_errors", return_value=["pinned tool SHA-256 differs: cargo"]):
            self.assertIn("pinned tool SHA-256 differs", " ".join(self.check()))

    def test_free_space_below_twenty_gib_is_refused(self):
        with patch.object(install.shutil, "disk_usage", return_value=shutil._ntuple_diskusage(100, 50, install.FLOOR - 1)):
            self.assertIn("free space below 20 GiB", self.check())

    def test_plist_missing_backend_and_development_flag_are_refused(self):
        for args in (["runner", "serve"], ["runner", "serve", "--backend", "macos-vm", "--development-allow-host-backend"]):
            with self.subTest(args=args):
                self.plist.write_bytes(plistlib.dumps({"UserName": "motor", "ProgramArguments": args}))
                self.assertTrue(any("backend" in error or "development flag" in error for error in self.check()))

    def test_uninstall_removes_only_owned_match_block(self):
        original = "Port 2222\n# provider settings\nMatch User provider\n    X11Forwarding no\n"
        managed = original + install.ssh_block()
        self.assertEqual(install.remove_block(managed), original)
        self.assertEqual(install.remove_block(install.remove_block(managed) + install.ssh_block()), original)
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            install.remove_block(managed + install.ssh_block())

    def test_install_and_update_sign_the_installed_vm_creator(self):
        manifest_path = self.root / "reviewed.json"
        manifest_path.write_text(json.dumps(self.manifest))
        bundle = self.root / "bundle"
        bundle.mkdir()
        runner, helper = self.root / "runner", self.root / "helper"
        runner.write_bytes(b"runner")
        helper.write_bytes(b"helper")
        key = self.root / "identity.pub"
        key.write_text("ssh-ed25519 AAAA\n")
        args = SimpleNamespace(state_root=self.state, image=self.image, tool_bundle=bundle, manifest=manifest_path,
                               runner=runner, vm_helper=helper, runner_key=key, instance="macos",
                               data_volume=None, operator_account=None, signing_team=None,
                               metrics_endpoint='https://metrics.example', metrics_host_id='monitor-1')
        account = SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())
        install.LEGACY_PLIST.touch()
        self.connect.unlink()
        output = io.StringIO()
        with patch.multiple(install, CONFIG=self.config, PLIST=self.plist, SSHD=self.sshd, SSH_ROOT=self.ssh, HOME=self.root / "home"), \
                patch.object(install, "create_account", return_value=account), patch.object(install.os, "chown") as chown, \
                patch.object(install.os, "fchown"), \
                patch.object(install, "configure_login", side_effect=[True, False]), \
                patch.object(install, "has_gui_session", side_effect=[True, False]), patch("sys.stdout", output), \
                patch.object(install, "run", side_effect=self.command) as run, \
                patch.object(install.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as subprocess_run:
            install.install(args, self.manifest)
            install.install(args, self.manifest)
        self.assertFalse(install.LEGACY_PLIST.exists())
        self.assertEqual(os.readlink(self.connect), str(self.config / "motor-runner"))
        self.assertEqual(self.plist.stat().st_mode & 0o777, 0o644)
        bootstraps = [call.args for call in run.call_args_list if call.args[:2] == ("/bin/launchctl", "bootstrap")]
        self.assertEqual(bootstraps, [("/bin/launchctl", "bootstrap", f"gui/{account.pw_uid}", self.plist)])
        self.assertEqual(output.getvalue().count("Restart required:"), 1)
        self.assertIn("no Aqua login session", output.getvalue())
        self.assertIn((self.plist, 0, 0), [call.args for call in chown.call_args_list])
        self.assertIn((self.plist.parent, account.pw_uid, account.pw_gid), [call.args for call in chown.call_args_list])
        stopped = [call.args[0] for call in subprocess_run.call_args_list]
        self.assertIn(["/bin/launchctl", "bootout", "system/" + install.LABEL], stopped)
        self.assertIn(["/bin/launchctl", "bootout", f"gui/{account.pw_uid}/" + install.LABEL], stopped)
        signed = [call for call in run.call_args_list if call.args[:2] == ("/usr/bin/codesign", "--force")]
        self.assertEqual(len(signed), 2)
        for call in signed:
            self.assertEqual(call.args, ("/usr/bin/codesign", "--force", "--sign", "-", "--entitlements",
                                         HERE / "virtualization.entitlements", self.config / "motor-macvm"))
        self.assertEqual(install.plist_errors(plistlib.loads(self.plist.read_bytes())), [])
        self.assertEqual(plistlib.loads(self.plist.read_bytes())['ProgramArguments'][-5:],
                         ['--metrics-push', '--metrics-endpoint', 'https://metrics.example', '--metrics-host-id', 'monitor-1'])

    def install_for_metrics(self, endpoint=None, host_id=None, data_volume=None, operator_account: Optional[str] = "operator",
                            signing_team=None):
        manifest_path = self.root / "reviewed.json"
        manifest_path.write_text(json.dumps(self.manifest))
        bundle = self.root / "bundle"
        bundle.mkdir(exist_ok=True)
        runner, helper = self.root / "runner", self.root / "helper"
        runner.write_bytes(b"runner")
        helper.write_bytes(b"helper")
        key = self.root / "identity.pub"
        key.write_text("ssh-ed25519 AAAA\n")
        args = SimpleNamespace(state_root=self.state, image=self.image, tool_bundle=bundle, manifest=manifest_path,
                               runner=runner, vm_helper=helper, runner_key=key, instance="macos",
                               data_volume=data_volume, operator_account=operator_account, signing_team=signing_team,
                               metrics_endpoint=endpoint, metrics_host_id=host_id)
        account = SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())
        with patch.multiple(install, CONFIG=self.config, PLIST=self.plist, SSHD=self.sshd, SSH_ROOT=self.ssh, HOME=self.root / "home"), \
                patch.object(install, "create_account", return_value=account), patch.object(install.os, "chown"), \
                patch.object(install.os, "fchown"), patch.object(install, "configure_login", return_value=False), \
                patch.object(install, "has_gui_session", return_value=False), patch("sys.stdout", io.StringIO()), \
                patch.object(install.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)):
            install.install(args, self.manifest)

    def test_signing_team_keeps_team_signatures_and_is_required_on_updates(self):
        # A first install (no record yet) may choose a signing team.
        (self.config / "install.json").unlink(missing_ok=True)
        teams = {"motor-runner": "TEAM1", "motor-macvm": "TEAM1"}
        with patch.object(install, "team_identifier", side_effect=lambda binary: teams[Path(binary).name]), \
                patch.object(install, "run", side_effect=self.command) as run:
            self.install_for_metrics(signing_team="TEAM1")
            # The team signature is kept: no ad-hoc re-sign of the helper.
            self.assertFalse([call for call in run.call_args_list if call.args[:2] == ("/usr/bin/codesign", "--force")])
            self.assertEqual(json.loads((self.config / "install.json").read_text())["signing_team"], "TEAM1")
            # Updates inherit the recorded team and refuse an ad-hoc build.
            teams["motor-runner"] = None
            with self.assertRaisesRegex(ValueError, "motor-runner must be signed by team TEAM1"):
                self.install_for_metrics(operator_account=None)
            teams["motor-runner"] = "TEAM1"
            with self.assertRaisesRegex(ValueError, "signing team cannot change on update"):
                self.install_for_metrics(operator_account=None, signing_team="TEAM2")
            self.install_for_metrics(operator_account=None)

    def test_reinstall_without_metrics_flags_keeps_installed_pair(self):
        installed = plistlib.loads(self.plist.read_bytes())
        installed['ProgramArguments'] += ['--metrics-push', '--metrics-endpoint', 'https://metrics.example:8443',
                                          '--metrics-host-id', 'monitor-1']
        self.plist.write_bytes(plistlib.dumps(installed))
        self.install_for_metrics()
        self.assertEqual(plistlib.loads(self.plist.read_bytes()), installed)

    def test_reinstall_with_metrics_pair_replaces_installed_pair(self):
        installed = plistlib.loads(self.plist.read_bytes())
        old_flags = ['--metrics-push', '--metrics-endpoint', 'https://old.example', '--metrics-host-id', 'old-host']
        self.plist.write_bytes(plistlib.dumps({**installed, 'ProgramArguments': installed['ProgramArguments'] + old_flags}))
        self.install_for_metrics('https://new.example:8443', 'new-host')
        self.assertEqual(plistlib.loads(self.plist.read_bytes())['ProgramArguments'], installed['ProgramArguments'] +
                         ['--metrics-push', '--metrics-endpoint', 'https://new.example:8443', '--metrics-host-id', 'new-host'])

    def test_first_install_without_metrics_flags_keeps_metrics_off(self):
        expected = plistlib.loads(self.plist.read_bytes())
        self.plist.unlink()
        self.install_for_metrics()
        installed = plistlib.loads(self.plist.read_bytes())
        self.assertEqual(installed, expected)
        self.assertFalse(any(arg.startswith('--metrics-') for arg in installed['ProgramArguments']))

    def test_reinstall_refuses_invalid_or_incomplete_installed_metrics(self):
        installed = plistlib.loads(self.plist.read_bytes())
        cases = [(['--metrics-push', '--metrics-endpoint', 'https://metrics.example'],
                  'metrics endpoint and host id must be supplied together'),
                 (['--metrics-endpoint', 'https://metrics.example', '--metrics-host-id', 'monitor-1'],
                  'incomplete LaunchAgent metrics settings'),
                 (['--metrics-push', '--metrics-endpoint', 'http://metrics.example', '--metrics-host-id', 'monitor-1'],
                  'metrics endpoint must be an HTTPS origin')]
        for flags, error in cases:
            for pair in ((None, None), ('https://new.example', 'new-host')):
                with self.subTest(flags=flags, pair=pair):
                    damaged = plistlib.dumps({**installed, 'ProgramArguments': installed['ProgramArguments'] + flags})
                    self.plist.write_bytes(damaged)
                    with patch.object(install, 'stop_jobs') as stop, self.assertRaisesRegex(ValueError, error):
                        self.install_for_metrics(*pair)
                    stop.assert_not_called()
                    self.assertEqual(self.plist.read_bytes(), damaged)

    def test_runtime_config_cannot_select_an_unchecked_helper_or_tool(self):
        path = self.config / "vm.toml"
        path.write_text(path.read_text().replace("motor-macvm", "unsigned-macvm"))
        self.assertIn("VM configuration differs from installed pins", self.check())

    def external_record(self):
        record = self.config / "install.json"
        installed = json.loads(record.read_text())
        installed["data_volume"] = DATA_UUID
        record.write_text(json.dumps(installed))
        plist = plistlib.loads(self.plist.read_bytes())
        plist["WorkingDirectory"] = str(self.state)
        plist["StandardOutPath"] = str(install.log_directory() / "daemon.stdout")
        plist["StandardErrorPath"] = str(install.log_directory() / "daemon.stderr")
        self.plist.write_bytes(plistlib.dumps(plist))
        return {"FilesystemType": "apfs", "VolumeUUID": DATA_UUID, "GlobalPermissionsEnabled": True,
                "RemovableMedia": False, "MountPoint": str(self.root), "Internal": False}

    def test_check_enforces_recorded_volume_and_automount_without_cli_flag(self):
        facts = self.external_record()
        with patch.object(install, "volume_info", return_value=facts) as probe, \
                patch.object(install, "preference", side_effect=lambda domain, key: True if domain == install.AUTODISKMOUNT else "motor"):
            self.assertEqual(self.check(), [])
            self.assertEqual([call.args[0] for call in probe.call_args_list], [self.state, self.image])
            for path in (self.state, self.image):
                with self.subTest(path=path), patch.object(install, "volume_info", side_effect=lambda item: {
                        **facts, "VolumeUUID": "wrong"} if item == path else facts):
                    self.assertIn("UUID does not match", " ".join(self.check()))
            for value in (False, None):
                with self.subTest(automount=value), patch.object(install, "preference", side_effect=lambda domain, key: value if domain == install.AUTODISKMOUNT else "motor"):
                    self.assertIn("rerun install to set AutomountDisksWithoutUserLogin", " ".join(self.check()))
            self.assertIn("uninstall first", " ".join(install.check_install(
                self.state, self.image, "macos", self.manifest, self.config, self.plist, self.sshd, self.ssh,
                data_volume="other")))
            plist = plistlib.loads(self.plist.read_bytes())
            del plist["WorkingDirectory"]
            self.plist.write_bytes(plistlib.dumps(plist))
            self.assertIn("LaunchAgent differs from installed configuration", self.check())

    def test_install_records_volume_and_operator_then_update_reuses_them(self):
        facts = self.external_record()
        (self.config / "install.json").unlink()
        with patch.object(install, "volume_info", return_value=facts), \
                patch.object(install, "run", side_effect=self.command) as run:
            self.install_for_metrics(data_volume=DATA_UUID)
            self.install_for_metrics(operator_account=None)
        installed = json.loads((self.config / "install.json").read_text())
        self.assertEqual(installed["data_volume"], DATA_UUID)
        self.assertEqual(installed["operator_account"], "operator")
        self.assertTrue(installed["automount_managed"])
        self.assertTrue((self.config / "autodiskmount-backup.plist").exists())
        writes = [call.args for call in run.call_args_list if call.args[:3] == (
            "/usr/bin/defaults", "write", install.AUTODISKMOUNT)]
        self.assertEqual(writes, [("/usr/bin/defaults", "write", install.AUTODISKMOUNT,
                                  install.AUTOMOUNT_KEY, "-bool", "true")] * 2)
        self.assertEqual(plistlib.loads(self.plist.read_bytes())["WorkingDirectory"], str(self.state))

    def test_check_missing_volume_reports_uuid_remedy_before_reading_state(self):
        facts = self.external_record()
        with patch.object(install, "volume_info", return_value={**facts, "VolumeUUID": "internal"}), \
                patch.object(install, "preference", side_effect=lambda domain, key: True if domain == install.AUTODISKMOUNT else "motor"), \
                patch.object(install.shutil, "disk_usage", side_effect=AssertionError("must not probe absent state")):
            errors = install.check_install(self.root / "missing/state", self.image, "macos", self.manifest,
                                           self.config, self.plist, self.sshd, self.ssh)
        self.assertIn("mount the recorded volume", " ".join(errors))

    def test_updates_refuse_changed_volume_or_operator_before_mutation(self):
        self.external_record()
        cases = [({"data_volume": "other"}, "data volume cannot change"),
                 ({"operator_account": "other"}, "operator account differs")]
        for kwargs, message in cases:
            with self.subTest(kwargs=kwargs), patch.object(install, "create_account") as create, \
                    self.assertRaisesRegex(ValueError, message):
                self.install_for_metrics(**kwargs)
            create.assert_not_called()
        with patch.dict(install.os.environ, {"SUDO_USER": "other"}), \
                self.assertRaisesRegex(ValueError, "invoked by the operator account"):
            self.install_for_metrics(operator_account=None)
        installed = json.loads((self.config / "install.json").read_text())
        del installed["data_volume"]
        (self.config / "install.json").write_text(json.dumps(installed))
        with self.assertRaisesRegex(ValueError, "uninstall first"):
            self.install_for_metrics(data_volume=DATA_UUID)

    def test_first_install_requires_explicit_matching_operator(self):
        (self.config / "install.json").unlink()
        for operator, message in ((None, "first install requires --operator-account"),
                                  ("other", "invoked by the operator account")):
            with self.subTest(operator=operator), self.assertRaisesRegex(ValueError, message):
                self.install_for_metrics(operator_account=operator)

    def test_legacy_record_defaults_to_original_operator(self):
        installed = json.loads((self.config / "install.json").read_text())
        del installed["operator_account"]
        (self.config / "install.json").write_text(json.dumps(installed))
        with patch.dict(install.os.environ, {"SUDO_USER": "rentamac"}):
            self.install_for_metrics(operator_account=None)
            self.assertEqual(json.loads((self.config / "install.json").read_text())["operator_account"], "rentamac")
        with self.assertRaisesRegex(ValueError, "invoked by the operator account"):
            self.install_for_metrics(operator_account=None)

    def test_restored_uninstall_inventory_allows_a_new_volume_and_operator(self):
        installed = json.loads((self.config / "install.json").read_text())
        installed["auto_login_managed"] = False
        (self.config / "install.json").write_text(json.dumps(installed))
        with patch.object(install, "CONFIG", self.config):
            self.assertIsNone(install.installation_record())


class DataVolumeTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.mount = Path(scratch.name)
        self.path = self.mount / "runner/state"
        self.facts = {"FilesystemType": "apfs", "VolumeUUID": DATA_UUID, "GlobalPermissionsEnabled": True,
                      "RemovableMedia": False, "MountPoint": str(self.mount), "Internal": False}

    def test_volume_facts_are_read_as_plist_for_the_existing_ancestor_device(self):
        def command(args, **kwargs):
            if args == ["/usr/bin/stat", "-f", "%Sd", str(self.mount)]:
                return subprocess.CompletedProcess(args, 0, "disk7s1\n")
            self.assertEqual(args, ["/usr/sbin/diskutil", "info", "-plist", "disk7s1"])
            return subprocess.CompletedProcess(args, 0, plistlib.dumps(self.facts))
        with patch.object(install.subprocess, "run", side_effect=command):
            self.assertEqual(install.volume_errors(self.path, DATA_UUID), [])

    def test_each_required_volume_fact_fails_closed_with_a_remedy(self):
        cases = [("FilesystemType", "hfs", "reformat the volume as APFS"),
                 ("VolumeUUID", "other", "mount the recorded volume"),
                 ("GlobalPermissionsEnabled", False, "sudo diskutil enableOwnership " + str(self.mount)),
                 ("RemovableMedia", True, "use a fixed-media APFS disk"),
                 ("MountPoint", None, "mount it and use paths beneath its mount point")]
        for key, value, message in cases:
            for missing in (False, True):
                with self.subTest(key=key, missing=missing):
                    facts = dict(self.facts)
                    if missing:
                        del facts[key]
                    else:
                        facts[key] = value
                    expected = message if key != "GlobalPermissionsEnabled" else "sudo diskutil enableOwnership"
                    with patch.object(install, "volume_info", return_value=facts):
                        self.assertIn(expected, " ".join(install.volume_errors(self.path, DATA_UUID)))

    def test_missing_mount_uses_parent_device_and_refuses_before_state_creation(self):
        with patch.object(install, "volume_info", return_value={
                **self.facts, "VolumeUUID": "internal", "MountPoint": "/", "Internal": True}) as probe:
            self.assertIn("UUID does not match", " ".join(install.volume_errors(self.path, DATA_UUID)))
            probe.assert_called_once_with(self.mount)
            self.assertFalse(self.path.exists())

    def test_resolved_paths_cannot_escape_the_recorded_mount_point(self):
        with patch.object(install, "volume_info", return_value={**self.facts, "MountPoint": str(self.path)}):
            self.assertIn("paths beneath its mount point", " ".join(install.volume_errors(self.mount, DATA_UUID)))

    def test_external_agent_pins_absolute_state_path_before_recursive_runner_open(self):
        state = Path("/Volumes/RunnerData/motor/state")
        plist = install.agent(Path("/installed/motor-runner"), state, "macos", Path("/installed/vm.toml"), data_volume=DATA_UUID)
        self.assertEqual(plist["WorkingDirectory"], str(state))
        args = plist["ProgramArguments"]
        self.assertEqual(args[args.index("--state-root") + 1], str(state))
        # launchd opens the logs itself and may not open files on an external volume.
        self.assertEqual(plist["StandardOutPath"], "/Users/motor/Library/Logs/motor-runner/daemon.stdout")
        self.assertEqual(plist["StandardErrorPath"], "/Users/motor/Library/Logs/motor-runner/daemon.stderr")
        internal = install.agent(Path("/runner"), state, "macos", Path("/vm.toml"))
        self.assertNotIn("WorkingDirectory", internal)
        self.assertEqual(internal["StandardErrorPath"], str(state / "daemon.stderr"))


class AutoLoginTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.home = self.root / "motor"
        self.keychain = self.home / "Library/Keychains/login.keychain-db"
        self.keychain.parent.mkdir(parents=True)
        self.keychain.write_bytes(b"original keychain")
        self.password = self.root / "password"
        self.backup = self.root / "backup.plist"
        self.kc = self.root / "kcpassword"
        self.kc.write_bytes(b"original obfuscated password")
        self.plist = self.home / "Library/LaunchAgents" / (install.LABEL + ".plist")
        self.legacy = self.root / "legacy.plist"
        self.sshd = self.root / "sshd_config"
        self.sshd.write_text("Port 2222\n" + install.ssh_block())
        self.user = "operator"
        self.automount = None
        self.default_keychain = str(self.keychain)
        self.account = SimpleNamespace(pw_uid=502, pw_gid=20)
        patches = [patch.multiple(install, HOME=self.home, PASSWORD=self.password, BACKUP=self.backup, CONFIG=self.root,
                                  KCPASSWORD=self.kc, PLIST=self.plist, LEGACY_PLIST=self.legacy, SSHD=self.sshd),
                   patch.object(install.os, "chown"), patch.object(install.os, "fchown"),
                   patch.object(install.pwd, "getpwnam", return_value=self.account),
                   patch.object(install, "run", side_effect=self.command),
                   patch.object(install.subprocess, "run", side_effect=self.native_command)]
        for item in patches:
            item.start()
            self.addCleanup(item.stop)
        self.run: Any = install.run
        self.native: Any = install.subprocess.run
        self.fchown: Any = install.os.fchown

    def command(self, *args):
        if args[0] == "/usr/bin/defaults" and args[2] == install.AUTODISKMOUNT:
            if args[1] == "export":
                return plistlib.dumps({} if self.automount is None else {install.AUTOMOUNT_KEY: self.automount}).decode()
            if args[1] == "write":
                self.assertEqual(args[3:5], (install.AUTOMOUNT_KEY, "-bool"))
                self.automount = args[5] == "true"
            else:
                self.assertEqual(args[1], "delete")
                self.automount = None
            return ""
        if args[:2] == ("/usr/bin/defaults", "export"):
            values = {} if self.user is None else {"autoLoginUser": self.user}
            return plistlib.dumps(values).decode()
        if args[:2] == ("/usr/bin/defaults", "write"):
            self.user = args[-1]
        elif args[:2] == ("/usr/bin/defaults", "delete"):
            self.user = None
        elif args[:len(install.as_motor("/usr/bin/security"))] == install.as_motor("/usr/bin/security"):
            rest = args[len(install.as_motor("/usr/bin/security")):]
            if rest == ("set-keychain-settings", self.keychain):
                return ""
            self.assertEqual(rest[:3], ("default-keychain", "-d", "user"))
            # As on macOS 27.2, reading or clearing a default that is unset, or
            # names a removed keychain file, fails with "A default keychain
            # could not be found."
            missing = not self.default_keychain or not Path(self.default_keychain).exists()
            if len(rest) == 3:
                if missing:
                    raise subprocess.CalledProcessError(1, args)
                return '"' + self.default_keychain + '"'
            self.assertEqual(rest[3], "-s")
            if len(rest) == 4 and missing:
                raise subprocess.CalledProcessError(1, args)
            self.default_keychain = str(rest[4]) if len(rest) == 5 else ""
        elif args[:2] == ("/usr/sbin/sshd", "-t"):
            pass
        elif args[:2] == ("/bin/launchctl", "print"):
            if not getattr(self, "gui_session", False):
                raise subprocess.CalledProcessError(113, args)
        else:
            self.fail("unexpected command: " + str(args))
        return ""

    def native_command(self, args, **kwargs):
        if args[:4] == ["/usr/bin/dscl", ".", "-passwd", "/Users/motor"]:
            self.assertTrue(args[4])
        elif args[:len(install.as_motor("/usr/bin/security", "create-keychain"))] == list(
                install.as_motor("/usr/bin/security", "create-keychain")):
            self.assertEqual(args[len(install.as_motor("/usr/bin/security", "create-keychain"))], "-p")
            Path(args[-1]).write_bytes(b"new keychain")
        elif args[:len(install.as_motor("/usr/bin/security", "unlock-keychain"))] == list(
                install.as_motor("/usr/bin/security", "unlock-keychain")):
            self.assertEqual(args[len(install.as_motor("/usr/bin/security", "unlock-keychain")):],
                             ["-p", self.password.read_text().strip(), str(self.keychain)])
        elif args == [str(arg) for arg in install.as_motor("/usr/bin/security", "show-keychain-info", self.keychain)]:
            return subprocess.CompletedProcess(args, 0, "", "Keychain: no-timeout")
        elif args[:2] == ["/bin/launchctl", "bootout"]:
            pass
        else:
            self.fail("unexpected subprocess: " + str(args))
        return subprocess.CompletedProcess(args, 0)

    def test_kcpassword_has_known_apple_xor_bytes_and_twelve_byte_padding(self):
        self.assertEqual(install.kcpassword("password"), bytes.fromhex("0de82150a5d3af8ea3b91f7d"))
        for password in ("", "a" * 11, "a" * 12, "a" * 23):
            encoded = install.kcpassword(password)
            self.assertEqual(len(encoded) % 12, 0)
            key = bytes.fromhex("7d895223d2bcddeaa3b91f")
            decoded = bytes(value ^ key[index % 11] for index, value in enumerate(encoded))
            self.assertEqual(decoded, password.encode() + b"\0" * (len(encoded) - len(password)))
            self.assertGreater(len(encoded), len(password))

    def test_uninstall_restores_previous_automount_value_or_absence_after_updates(self):
        for previous in (None, False, True):
            with self.subTest(previous=previous):
                self.automount = previous
                install.configure_login(self.account)
                install.configure_automount()
                self.assertIs(self.automount, True)
                (self.root / "install.json").write_text(json.dumps({
                    "auto_login_managed": True, "automount_managed": True, "data_volume": DATA_UUID}))
                install.configure_automount()
                install.uninstall()
                self.assertIs(self.automount, previous)
                self.assertFalse((self.root / "autodiskmount-backup.plist").exists())
                self.assertFalse(json.loads((self.root / "install.json").read_text())["automount_managed"])
                install.uninstall()
                self.assertIs(self.automount, previous)

    def test_missing_managed_automount_backup_refuses_update_and_uninstall(self):
        (self.root / "install.json").write_text(json.dumps({"automount_managed": True}))
        with self.assertRaisesRegex(ValueError, "automount backup is missing.*updating"):
            install.configure_automount()
        with self.assertRaisesRegex(ValueError, "automount backup is missing.*uninstalling"):
            install.uninstall()
        self.assertIsNone(self.automount)

    def test_first_keychain_is_not_created_under_a_live_session(self):
        self.gui_session = True
        with self.assertRaisesRegex(ValueError, "motor is logged in"):
            install.configure_login(self.account)
        self.assertFalse(any("create-keychain" in call.args[0] for call in self.native.call_args_list))

    def test_login_setup_reuses_password_and_preserves_original_backup_on_update(self):
        self.assertTrue(install.configure_login(self.account))
        password = self.password.read_bytes()
        original_backup = self.backup.read_bytes()
        self.assertEqual(self.user, "motor")
        self.assertEqual(self.kc.read_bytes(), install.kcpassword(password.decode().strip()))
        self.assertEqual(self.keychain.read_bytes(), b"new keychain")
        self.assertEqual(self.default_keychain, str(self.keychain))
        self.assertFalse(install.configure_login(self.account))
        self.assertEqual(self.password.read_bytes(), password)
        self.assertEqual(self.backup.read_bytes(), original_backup)
        creates = [call for call in self.native.call_args_list if "create-keychain" in call.args[0]]
        self.assertEqual(len(creates), 1, "updates must not destroy VM Secure Enclave keys")
        for path in (self.password, self.backup, self.kc):
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertTrue(self.fchown.called)
        self.assertTrue(all(call.args[1:] == (0, 0) for call in self.fchown.call_args_list))

    def test_login_keychain_is_unlocked_without_auto_lock_on_setup_and_update(self):
        for _ in range(2):
            self.native.reset_mock()
            self.run.reset_mock()
            install.configure_login(self.account)
            unlock = [call.args[0] for call in self.native.call_args_list if "unlock-keychain" in call.args[0]]
            self.assertEqual(len(unlock), 1)
            self.assertEqual(unlock, [[str(arg) for arg in install.as_motor(
                "/usr/bin/security", "unlock-keychain", "-p", self.password.read_text().strip(), self.keychain)]])
            self.run.assert_any_call(*install.as_motor("/usr/bin/security", "set-keychain-settings", self.keychain))
            self.assertEqual(install.keychain_errors(), [])
            self.assertEqual(self.native.call_args.args[0], [str(arg) for arg in install.as_motor(
                "/usr/bin/security", "show-keychain-info", self.keychain)])

    def test_motor_commands_run_in_motors_own_launchd_session(self):
        # macOS 27.2 rejects the right password for motor's login keychain
        # when the command comes from the installing operator's session.
        self.assertEqual(install.as_motor("/usr/bin/security", "list-keychains")[:3],
                         ("/bin/launchctl", "asuser", str(self.account.pw_uid)))

    def test_failed_unlock_does_not_expose_password_and_can_be_retried(self):
        def fail_unlock(args, **kwargs):
            if "unlock-keychain" in args:
                return subprocess.CompletedProcess(args, 1)
            return self.native_command(args, **kwargs)
        with patch.object(install.subprocess, "run", side_effect=fail_unlock) as failed_run:
            with self.assertRaisesRegex(ValueError, "setup failed") as caught:
                install.configure_login(self.account)
        self.assertNotIn(self.password.read_text().strip(), str(caught.exception))
        self.assertTrue(plistlib.loads(self.backup.read_bytes())["keychain_ready"])
        self.assertEqual(len([call for call in failed_run.call_args_list if "create-keychain" in call.args[0]]), 1)
        self.native.reset_mock()
        install.configure_login(self.account)
        self.assertFalse(any("create-keychain" in call.args[0] for call in self.native.call_args_list))

    def test_uninstall_restores_login_files_and_stops_both_jobs_idempotently(self):
        self.default_keychain = str(self.root / "other.keychain-db")
        Path(self.default_keychain).touch()
        install.configure_login(self.account)
        saved_password = self.password.read_text().strip()
        (self.root / "install.json").write_text(json.dumps({"auto_login_managed": True}))
        self.plist.parent.mkdir()
        self.plist.touch()
        self.legacy.touch()
        local_items = self.keychain.parent / "5EE0ADC4-0666-5049-9475-81CC1E6A9984"
        local_items.mkdir()
        (local_items / "user.kb").write_bytes(b"bound to the revoked password")
        other = self.keychain.parent / "not-a-uuid"
        other.mkdir()
        install.uninstall()
        self.assertFalse(local_items.exists())
        self.assertTrue(other.exists())
        self.assertEqual(self.user, "operator")
        self.assertEqual(self.kc.read_bytes(), b"original obfuscated password")
        self.assertEqual(self.keychain.read_bytes(), b"original keychain")
        self.assertEqual(self.default_keychain, str(self.root / "other.keychain-db"))
        self.assertFalse(self.password.exists())
        self.assertFalse(self.backup.exists())
        self.assertFalse(json.loads((self.root / "install.json").read_text())["auto_login_managed"])
        self.assertFalse(self.plist.exists())
        self.assertFalse(self.legacy.exists())
        self.assertEqual(self.sshd.read_text(), "Port 2222\n")
        password_changes = [call.args[0][-1] for call in self.native.call_args_list
                            if call.args[0][:2] == ["/usr/bin/dscl", "."]]
        self.assertNotEqual(password_changes[-1], saved_password)
        self.assertGreaterEqual(len(password_changes[-1]), 32)
        stopped = [call.args[0] for call in self.native.call_args_list if "bootout" in call.args[0]]
        self.assertIn(["/bin/launchctl", "bootout", "gui/502/" + install.LABEL], stopped)
        install.uninstall()
        self.assertEqual(self.user, "operator")

    def test_uninstall_restores_absence_of_previous_auto_login_and_keychain(self):
        self.user = None
        self.kc.unlink()
        self.keychain.unlink()
        self.default_keychain = ""
        install.configure_login(self.account)
        install.uninstall()
        self.assertIsNone(self.user)
        self.assertFalse(self.kc.exists())
        self.assertFalse(self.keychain.exists())
        # motor is left with no usable default keychain, as before the install.
        with self.assertRaises(subprocess.CalledProcessError):
            install.run(*install.as_motor("/usr/bin/security", "default-keychain", "-d", "user"))

    def test_missing_saved_password_does_not_rotate_existing_keychain(self):
        install.configure_login(self.account)
        self.password.unlink()
        with self.assertRaisesRegex(ValueError, "password is missing"):
            install.configure_login(self.account)
        self.assertEqual(self.keychain.read_bytes(), b"new keychain")

    def test_previous_motor_auto_login_is_disabled_instead_of_restored(self):
        self.user = "motor"
        install.configure_login(self.account)
        output = io.StringIO()
        with patch("sys.stdout", output):
            install.uninstall()
        self.assertIsNone(self.user)
        self.assertFalse(self.kc.exists())
        self.assertIn("Auto-login is now off", output.getvalue())
        self.assertIn("re-enable auto-login for their own account", output.getvalue())

    def test_missing_backup_is_not_silently_replaced_or_reported_as_restored(self):
        install.configure_login(self.account)
        self.backup.unlink()
        (self.root / "install.json").write_text(json.dumps({"auto_login_managed": True}))
        with self.assertRaisesRegex(ValueError, "backup is missing"):
            install.configure_login(self.account)
        with self.assertRaisesRegex(ValueError, "backup is missing"):
            install.restore_login()
        self.assertTrue(self.password.exists())

    def test_interrupted_private_write_preserves_the_original(self):
        with patch.object(install.os, "replace", side_effect=OSError("interrupted")):
            with self.assertRaisesRegex(OSError, "interrupted"):
                install.private_write(self.kc, b"replacement")
        self.assertEqual(self.kc.read_bytes(), b"original obfuscated password")
        self.assertEqual(list(self.root.glob(".kcpassword.*")), [])

    def test_password_command_failure_never_exposes_the_password(self):
        with patch.object(install.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)):
            with self.assertRaisesRegex(ValueError, "setup failed") as caught:
                install.configure_login(self.account)
        self.assertNotIn(self.password.read_text().strip(), str(caught.exception))
        self.assertEqual(self.keychain.read_bytes(), b"original keychain")

    def test_failed_keychain_creation_can_be_retried_without_losing_backup(self):
        def fail_create(args, **kwargs):
            if "create-keychain" in args:
                return subprocess.CompletedProcess(args, 1)
            return self.native_command(args, **kwargs)
        with patch.object(install.subprocess, "run", side_effect=fail_create):
            with self.assertRaisesRegex(ValueError, "setup failed"):
                install.configure_login(self.account)
        saved = self.password.read_bytes()
        backup = plistlib.loads(self.backup.read_bytes())
        self.assertEqual(backup["keychain"], b"original keychain")
        self.assertFalse(backup["keychain_ready"])
        self.assertTrue(install.configure_login(self.account))
        self.assertEqual(self.password.read_bytes(), saved)
        install.uninstall()
        self.assertEqual(self.keychain.read_bytes(), b"original keychain")

    def test_private_files_refuse_symlinks(self):
        self.password.symlink_to(self.kc)
        with self.assertRaises(OSError):
            install.private_write(self.password, b"secret")
        self.assertEqual(self.kc.read_bytes(), b"original obfuscated password")

    def test_filevault_on_refuses_before_any_account_or_login_change(self):
        with patch.object(install, "run", return_value="FileVault is On."), \
                patch.object(install, "create_account") as create:
            with self.assertRaisesRegex(ValueError, "FileVault must be off"):
                install.install(SimpleNamespace(), ready())
        create.assert_not_called()
        self.assertFalse(self.backup.exists())

    def test_gui_probe_failure_is_not_a_login_session(self):
        with patch.object(install, "run", side_effect=subprocess.CalledProcessError(113, "launchctl")) as run:
            self.assertFalse(install.has_gui_session(self.account))
        run.assert_called_once_with("/bin/launchctl", "print", "gui/502")

    def test_stop_failure_does_not_remove_a_still_loaded_job(self):
        self.legacy.touch()
        with patch.object(install.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)), \
                patch.object(install, "run", return_value="state = running"):
            with self.assertRaisesRegex(ValueError, "could not stop launchd job"):
                install.stop_jobs(self.account)
        self.assertTrue(self.legacy.exists())


class OfflineImageTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.bundle = self.root / "image"
        self.bundle.mkdir()
        for name in image.IMAGE_FILES:
            (self.bundle / name).write_bytes(b"ungranted disk")
        self.disk = self.bundle / "disk.img"
        self.requirement = 'cdhash H"0123456789abcdef0123456789abcdef01234567"'
        self.failure = None
        self.data_roles = ["Data"]
        self.commands = []
        self.guest_data = None
        native = patch.object(image.subprocess, "run", side_effect=self.command)
        native.start()
        self.addCleanup(native.stop)

    def command(self, args, **kwargs):
        self.commands.append(args)
        step = Path(args[0]).name
        if step == "hdiutil":
            step = args[1]
        if step == self.failure:
            raise subprocess.CalledProcessError(1, args)
        output = b""
        if step == "attach":
            self.assertIn("-nomount", args)
            self.active_disk = Path(args[-1])
            self.assertEqual(self.active_disk.name, "disk.img")
            output = plistlib.dumps({"system-entities": [{"dev-entry": "/dev/disk42"}, {"dev-entry": "/dev/disk42s2"}]})
        elif step == "diskutil":
            self.assertEqual(args[1:], ["apfs", "list", "-plist"])
            output = plistlib.dumps({"Containers": [
                {"PhysicalStores": [{"DeviceIdentifier": "disk0s2"}],
                 "Volumes": [{"DeviceIdentifier": "disk1s1", "Roles": ["Data"]}]},
                {"PhysicalStores": [{"DeviceIdentifier": "disk42s2"}],
                 "Volumes": [{"DeviceIdentifier": "disk43s1", "Roles": ["System"]},
                             {"DeviceIdentifier": "disk43s5", "Roles": self.data_roles}]}]})
        elif step == "mount_apfs":
            self.assertEqual(args[-2], "/dev/disk43s5")
            self.mount = Path(args[-1])
            if self.guest_data is not None:
                shutil.copytree(self.guest_data, self.mount, dirs_exist_ok=True)
            else:
                agent = self.mount / "opt/motor/bin/motor-macagent"
                agent.parent.mkdir(parents=True)
                agent.write_bytes(b"installed agent, not build output")
                database = self.mount / "Library/Application Support/com.apple.TCC/TCC.db"
                database.parent.mkdir(parents=True)
                database.write_bytes(b"system TCC database")
        elif step == "codesign":
            self.assertEqual(args, ["/usr/bin/codesign", "-d", "-r-", str(self.mount / "opt/motor/bin/motor-macagent")])
            self.assertEqual(kwargs["stderr"], subprocess.STDOUT)
            output = "Executable=installed agent\n# designated => " + self.requirement + "\n"
            if self.guest_data is not None and Path(args[-1]).read_bytes() == b"replacement signed agent":
                output = "designated => " + self.swap_requirement + "\n"
        elif step == "csreq":
            self.assertEqual(args[1::2], ["-r", "-b"])
            expected = self.swap_requirement if self.guest_data is not None else self.requirement
            self.assertEqual(Path(args[2]).read_text(), expected + "\n")
            Path(args[4]).write_bytes(bytes.fromhex("fade0c000102"))
        elif step == "sqlite3":
            self.assertEqual(args[1], "-bail")
            self.assertEqual(Path(args[2]), self.mount / "Library/Application Support/com.apple.TCC/TCC.db")
            self.active_disk.write_bytes(b"granted disk")
        elif step == "detach":
            self.assertEqual(args, ["/usr/bin/hdiutil", "detach", "/dev/disk42"])
            if hasattr(self, "mount") and self.mount.exists():
                shutil.rmtree(self.mount)
                self.mount.mkdir()
        elif step == "umount":
            self.assertEqual(args, ["/sbin/umount", str(self.mount)])
            if self.guest_data is not None:
                shutil.rmtree(self.guest_data)
                shutil.copytree(self.mount, self.guest_data)
            shutil.rmtree(self.mount)
            self.mount.mkdir()
        elif step == "cp":
            self.assertEqual(args[:2], ["/bin/cp", "-c"])
            shutil.copyfile(args[-2], args[-1])
        elif step == "lsof":
            return subprocess.CompletedProcess(args, 1, b"", b"")
        elif step != "sync":
            self.fail("unexpected native command: " + str(args))
        return subprocess.CompletedProcess(args, 0, output, b"")

    def test_finalize_disables_and_checks_the_build_account_before_granting(self):
        order = []
        with patch.object(image, "disable_build_account", side_effect=lambda mount: order.append("disable")), \
                patch.object(image, "require_locked_build_account", side_effect=lambda mount: order.append("check")):
            image.grant_agent_access(self.disk, lock_build_account=True)
            self.assertEqual(order, ["disable", "check"])
            # The grant follows the check, so a refused check grants nothing.
            self.assertEqual(sum(args[0] == "/usr/bin/sqlite3" for args in self.commands), 1)
            order.clear()
            image.grant_agent_access(self.disk)
            self.assertEqual(order, [])
        with patch.object(image, "disable_build_account"), \
                patch.object(image, "require_locked_build_account", side_effect=ValueError("not provably locked")):
            before = sum(args[0] == "/usr/bin/sqlite3" for args in self.commands)
            with self.assertRaisesRegex(ValueError, "not provably locked"):
                image.grant_agent_access(self.disk, lock_build_account=True)
            self.assertEqual(sum(args[0] == "/usr/bin/sqlite3" for args in self.commands), before)

    def test_offline_grants_use_installed_signature_and_both_services(self):
        record = image.grant_agent_access(self.disk)
        self.assertEqual(record, {"services": ["kTCCServiceSystemPolicyNetworkVolumes", "kTCCServiceSystemPolicyAllFiles"],
                                  "requirement": self.requirement})
        self.assertIn("-readwrite", self.commands[0])
        self.assertEqual(next(args[1:3] for args in self.commands if args[0] == "/sbin/mount_apfs"), ["-o", "nobrowse"])
        sql = next(args[-1] for args in self.commands if args[0] == "/usr/bin/sqlite3")
        expected = "BEGIN IMMEDIATE;\n"
        for service in ("kTCCServiceSystemPolicyNetworkVolumes", "kTCCServiceSystemPolicyAllFiles"):
            expected += ("INSERT OR REPLACE INTO access "
                         "(service, client, client_type, auth_value, auth_reason, auth_version, csreq, flags, last_modified) "
                         f"VALUES ('{service}', '/opt/motor/bin/motor-macagent', 1, 2, 4, 1, X'fade0c000102', 0, "
                         "CAST(strftime('%s','now') AS INTEGER));\n")
        self.assertEqual(sql, expected + "COMMIT;")
        self.assertEqual([Path(args[0]).name for args in self.commands[-3:]], ["sync", "umount", "hdiutil"])
        self.assertFalse(self.mount.exists())

    def test_readonly_inspection_never_writes_the_disk(self):
        with image.mounted_data(self.disk) as mount:
            self.assertEqual(image.agent_requirement(mount), self.requirement)
        self.assertIn("-readonly", self.commands[0])
        self.assertEqual(next(args[2] for args in self.commands if args[0] == "/sbin/mount_apfs"), "nobrowse,rdonly")
        self.assertEqual(self.disk.read_bytes(), b"ungranted disk")
        self.assertFalse(any(Path(args[0]).name in ("sqlite3", "csreq") for args in self.commands))

    def test_every_post_attach_failure_detaches_and_never_returns_a_grant(self):
        for failure in ("diskutil", "mount_apfs", "codesign", "csreq", "sqlite3", "sync", "umount", "detach"):
            self.failure = failure
            self.commands.clear()
            with self.subTest(step=failure), self.assertRaises(subprocess.CalledProcessError):
                image.grant_agent_access(self.disk)
            self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])
            if failure in ("codesign", "csreq", "sqlite3", "sync"):
                self.assertEqual(self.commands[-2], ["/sbin/umount", str(self.mount)])

    def test_missing_data_volume_or_requirement_fails_closed_and_detaches(self):
        self.data_roles = ["System"]
        with self.assertRaisesRegex(ValueError, "exactly one APFS Data"):
            image.grant_agent_access(self.disk)
        self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])
        self.data_roles = ["Data"]
        self.requirement = ""
        with self.assertRaisesRegex(ValueError, "no designated"):
            image.grant_agent_access(self.disk)
        self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])
        self.assertFalse(any(args[0] == "/usr/bin/sqlite3" for args in self.commands))

    def finalize(self):
        manifest = ready()
        path = self.root / "toolchain.json"
        path.write_text(json.dumps(manifest))
        (self.root / "guest-sealed.json").write_text(json.dumps({"manifest_sha256": parity.digest(path),
                                                               "remote_login": "Off", "guest_services": service_receipt()}))
        args = SimpleNamespace(work=self.root, image=self.bundle, manifest=path)
        with patch.object(image, "require_committed"), patch("sys.stdout", io.StringIO()):
            image.finalize(args, manifest)

    def test_finalize_hashes_the_granted_disk_and_records_requirement(self):
        self.finalize()
        pin = json.loads((self.bundle / "image.json").read_text())
        self.assertEqual(pin["files"]["disk.img"], hashlib.sha256(b"granted disk").hexdigest())
        self.assertEqual(pin["tcc"], {"services": ["kTCCServiceSystemPolicyNetworkVolumes", "kTCCServiceSystemPolicyAllFiles"],
                                      "requirement": self.requirement})
        self.assertEqual(pin["guest_services"], service_receipt())
        self.assertEqual([Path(args[0]).name for args in self.commands[-5:]], ["hdiutil", "lsof", "lsof", "lsof", "lsof"])
        self.assertEqual((self.bundle / "disk.img").stat().st_mode & 0o222, 0)

    def test_failed_grant_does_not_finalize_the_image(self):
        self.failure = "sqlite3"
        with self.assertRaises(subprocess.CalledProcessError):
            self.finalize()
        self.assertFalse((self.bundle / "image.json").exists())
        self.assertNotEqual(self.disk.stat().st_mode & 0o222, 0)

    def test_image_check_compares_current_installed_requirement_readonly(self):
        self.finalize()
        self.commands.clear()
        manifest_sha = parity.digest(self.root / "toolchain.json")
        self.assertEqual(install.image_errors(self.bundle, manifest_sha), [])
        self.assertIn("-readonly", self.commands[0])
        self.requirement = 'cdhash H"changed"'
        self.assertIn("requirement differs from sealed TCC grant", " ".join(install.image_errors(self.bundle, manifest_sha)))
        self.assertFalse(any(args[0] == "/usr/bin/sqlite3" for args in self.commands))

    def test_briefly_busy_unmount_is_retried(self):
        busy = [2]

        def busy_then_free(args, **kwargs):
            if args[0] == "/sbin/umount" and busy[0]:
                busy[0] -= 1
                self.commands.append(args)
                return subprocess.CompletedProcess(args, 1, b"", b"Resource busy")
            return self.command(args, **kwargs)
        with patch.object(image.subprocess, "run", side_effect=busy_then_free), patch.object(image.time, "sleep"):
            with image.mounted_data(self.disk) as mount:
                self.assertTrue(mount.is_dir())
        self.assertEqual(sum(args[0] == "/sbin/umount" for args in self.commands), 3)
        self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])

    def test_failed_unmount_and_detach_never_recursively_delete_guest_data(self):
        def fail_cleanup(args, **kwargs):
            if args[0] == "/sbin/umount" or args[:2] == ["/usr/bin/hdiutil", "detach"]:
                self.commands.append(args)
                raise subprocess.CalledProcessError(1, args)
            return self.command(args, **kwargs)
        with patch.object(image.subprocess, "run", side_effect=fail_cleanup):
            with self.assertRaises(OSError):
                image.grant_agent_access(self.disk)
        self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])
        self.assertEqual((self.mount / "opt/motor/bin/motor-macagent").read_bytes(), b"installed agent, not build output")
        shutil.rmtree(self.mount)

    def swap_inputs(self):
        old = ready()
        old["agent"]["sha256"] = hashlib.sha256(b"installed agent, not build output").hexdigest()
        original = json.dumps(old, indent=2) + "\n"
        manifest_sha = hashlib.sha256(original.encode()).hexdigest()
        image.write_seal(self.bundle, manifest_sha, old["ipsw"], old["agent"],
                         {"services": list(image.TCC_SERVICES), "requirement": self.requirement})
        self.guest_data = self.root / "guest-data"
        prefix = self.guest_data / "opt/motor"
        (prefix / "bin").mkdir(parents=True)
        (prefix / "bin/motor-macagent").write_bytes(b"installed agent, not build output")
        (prefix / "toolchain.json").write_text(original)
        (prefix / "agent.sha256").write_text(old["agent"]["sha256"] + "\n")
        (prefix / "sealed.json").write_text(json.dumps({"manifest_sha256": manifest_sha,
                                                        "agent_sha256": old["agent"]["sha256"], "remote_login": "Off"}))
        database = self.guest_data / "Library/Application Support/com.apple.TCC/TCC.db"
        database.parent.mkdir(parents=True)
        database.write_bytes(b"system TCC database")
        manifest = copy.deepcopy(old)
        manifest["agent"] = {"commit": "b" * 40, "sha256": hashlib.sha256(b"replacement signed agent").hexdigest()}
        path = self.root / "replacement-toolchain.json"
        path.write_text(json.dumps(manifest, indent=2) + "\n")
        binary = self.root / "new-agent"
        binary.write_bytes(b"replacement signed agent")
        self.swap_requirement = 'cdhash H"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"'
        self.swap_args = SimpleNamespace(source=self.bundle, destination=self.root / "swapped", agent=binary, manifest=path)
        return manifest

    def swap(self, manifest):
        with patch.object(image, "require_committed"), patch.object(image.os, "chown") as chown, patch("sys.stdout", io.StringIO()):
            image.agent_swap(self.swap_args, manifest)
        return chown

    def test_agent_swap_preserves_measured_service_policy_metadata(self):
        manifest = self.swap_inputs()
        receipt = service_receipt()
        metadata = self.bundle / "image.json"
        pin = json.loads(metadata.read_text())
        pin["guest_services"] = receipt
        metadata.chmod(0o644)
        metadata.write_text(json.dumps(pin))
        metadata.chmod(0o444)
        assert self.guest_data is not None
        guest_seal = self.guest_data / "opt/motor/sealed.json"
        sealed = json.loads(guest_seal.read_text())
        sealed["guest_services"] = receipt
        guest_seal.write_text(json.dumps(sealed))
        self.swap(manifest)
        self.assertEqual(json.loads((self.swap_args.destination / "image.json").read_text())["guest_services"], receipt)
        self.assertEqual(json.loads(guest_seal.read_text())["guest_services"], receipt)

    def test_agent_swap_clones_replaces_atomically_and_reseals_without_touching_source(self):
        manifest = self.swap_inputs()
        before = {path.name: (path.read_bytes(), path.stat().st_mode) for path in self.bundle.iterdir()}
        destination = self.swap_args.destination
        real_replace = os.replace

        def replace(staged, installed):
            self.assertFalse(destination.exists())
            self.assertEqual(staged.parent, installed.parent)
            self.assertEqual(installed.read_bytes(), b"installed agent, not build output")
            self.assertEqual(staged.read_bytes(), b"replacement signed agent")
            self.assertEqual(staged.stat().st_mode & 0o777, 0o555)
            real_replace(staged, installed)

        with patch.object(image.os, "replace", side_effect=replace) as atomic:
            chown = self.swap(manifest)
        atomic.assert_called_once()
        chown.assert_called_once_with(atomic.call_args.args[0], 0, 0)
        self.assertEqual(before, {path.name: (path.read_bytes(), path.stat().st_mode) for path in self.bundle.iterdir()})
        copies = [args for args in self.commands if args[0] == "/bin/cp"]
        self.assertEqual([Path(args[-2]).name for args in copies], list(image.IMAGE_FILES))
        self.assertTrue(all(args[1] == "-c" for args in copies))
        pin = json.loads((destination / "image.json").read_text())
        self.assertEqual(pin["agent"], manifest["agent"])
        self.assertEqual(pin["ipsw"], manifest["ipsw"])
        self.assertEqual(pin["manifest_sha256"], parity.digest(self.swap_args.manifest))
        self.assertEqual(pin["tcc"], {"services": list(image.TCC_SERVICES), "requirement": self.swap_requirement})
        self.assertEqual((destination / "agent.sha256").read_text(), manifest["agent"]["sha256"] + "\n")
        for name, sha in pin["files"].items():
            self.assertEqual(sha, hashlib.sha256((destination / name).read_bytes()).hexdigest())
        self.assertEqual(destination.stat().st_mode & 0o777, 0o555)
        self.assertTrue(all(path.stat().st_mode & 0o777 == 0o444 for path in destination.iterdir()))
        assert self.guest_data is not None
        self.assertEqual((self.guest_data / "opt/motor/toolchain.json").read_bytes(), self.swap_args.manifest.read_bytes())
        self.assertEqual((self.guest_data / "opt/motor/agent.sha256").read_text(), manifest["agent"]["sha256"] + "\n")
        receipt = json.loads((self.guest_data / "opt/motor/sealed.json").read_text())
        self.assertEqual(receipt, {"manifest_sha256": pin["manifest_sha256"], "agent_sha256": manifest["agent"]["sha256"], "remote_login": "Off"})
        self.assertEqual([args[2] for args in self.commands if args[:2] == ["/usr/bin/hdiutil", "attach"]], ["-nomount", "-nomount"])
        self.assertFalse(self.mount.exists())

    def test_agent_swap_grants_a_source_sealed_before_tcc_records(self):
        manifest = self.swap_inputs()
        record = self.bundle / "image.json"
        self.bundle.chmod(0o755)
        record.chmod(0o644)
        legacy = json.loads(record.read_text())
        del legacy["tcc"]
        record.write_text(json.dumps(legacy, indent=2) + "\n")
        record.chmod(0o444)
        self.bundle.chmod(0o555)
        self.swap(manifest)
        pin = json.loads((self.swap_args.destination / "image.json").read_text())
        self.assertEqual(pin["tcc"], {"services": list(image.TCC_SERVICES), "requirement": self.swap_requirement})

    def test_agent_swap_rejects_bad_binary_and_unsealed_or_corrupt_source_before_attach(self):
        manifest = self.swap_inputs()
        self.swap_args.agent.write_bytes(b"not the reviewed binary")
        with self.assertRaisesRegex(ValueError, "agent SHA-256"):
            self.swap(manifest)
        self.swap_args.agent.write_bytes(b"replacement signed agent")
        self.bundle.chmod(0o755)
        with self.assertRaisesRegex(ValueError, "read-only sealed"):
            self.swap(manifest)
        self.bundle.chmod(0o555)
        self.disk.chmod(0o644)
        self.disk.write_bytes(b"tampered disk")
        self.disk.chmod(0o444)
        with self.assertRaisesRegex(ValueError, "digest differs: disk.img"):
            self.swap(manifest)
        self.assertFalse(any(args[0] in ("/bin/cp", "/usr/bin/hdiutil") for args in self.commands))
        self.assertFalse(self.swap_args.destination.exists())

    def test_agent_swap_refuses_non_agent_manifest_changes_and_cleans_staging(self):
        manifest = self.swap_inputs()
        manifest["ipsw"]["build"] = "other build"
        self.swap_args.manifest.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "only agent changes"):
            self.swap(manifest)
        self.assertFalse(self.swap_args.destination.exists())
        self.assertEqual(list(self.root.glob(".swapped-*")), [])
        self.assertFalse(self.mount.exists())

    def test_agent_swap_failures_never_publish_and_always_detach_after_attach(self):
        for failure in ("cp", "attach", "diskutil", "mount_apfs", "codesign", "csreq", "sqlite3", "sync", "umount", "detach"):
            with self.subTest(step=failure), tempfile.TemporaryDirectory() as directory:
                original_root = self.root
                self.root = Path(directory)
                self.bundle = self.root / "image"
                self.bundle.mkdir()
                for name in image.IMAGE_FILES:
                    (self.bundle / name).write_bytes(b"ungranted disk")
                manifest = self.swap_inputs()
                self.commands.clear()
                self.failure = failure
                with self.assertRaises(subprocess.CalledProcessError):
                    self.swap(manifest)
                self.assertFalse(self.swap_args.destination.exists())
                self.assertEqual(list(self.root.glob(".swapped-*")), [])
                if failure not in ("cp", "attach"):
                    self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])
                self.root = original_root

    def build_account(self, **changes):
        assert self.guest_data is not None
        user = {"name": ["motorbuild"], "shell": ["/usr/bin/false"], "passwd": ["*"],
                "home": ["/Users/motorbuild"], "generateduid": ["BUILD-UUID"]}
        user.update(changes)
        node = self.guest_data / "private/var/db/dslocal/nodes/Default"
        (node / "users").mkdir(parents=True, exist_ok=True)
        (node / "users/motorbuild.plist").write_bytes(plistlib.dumps(user))
        (node / "groups").mkdir(exist_ok=True)
        (node / "groups/admin.plist").write_bytes(plistlib.dumps({"users": [], "groupmembers": ["BUILD-UUID"]}))
        return node / "users/motorbuild.plist"

    def test_agent_swap_preserves_provably_locked_build_account_and_reports_admin_membership(self):
        manifest = self.swap_inputs()
        account = self.build_account(authentication_authority=[";DisabledUser;;ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>"],
                                     ShadowHashData=[b"disabled password material"])
        before = account.read_bytes()
        with patch("sys.stdout", io.StringIO()) as output:
            image.require_locked_build_account(self.guest_data)
        self.assertIn("direct admin membership: present", output.getvalue())
        self.assertIn("account: locked", output.getvalue())
        self.swap(manifest)
        self.assertEqual(account.read_bytes(), before)
        self.assertTrue(self.swap_args.destination.exists())

    def test_agent_swap_refuses_enabled_shadowhash_even_with_password_marker(self):
        manifest = self.swap_inputs()
        self.build_account(authentication_authority=[";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>"],
                           ShadowHashData=[b"usable password material"])
        with self.assertRaisesRegex(ValueError, "authentication_authority contains an enabled"):
            self.swap(manifest)
        self.assertFalse(self.swap_args.destination.exists())
        self.assertEqual(list(self.root.glob(".swapped-*")), [])
        self.assertFalse(any(args[0] == "/usr/bin/sqlite3" for args in self.commands))
        self.assertEqual(self.commands[-1], ["/usr/bin/hdiutil", "detach", "/dev/disk42"])

    def test_agent_swap_disables_legacy_build_account_only_when_asked(self):
        manifest = self.swap_inputs()
        account = self.build_account(authentication_authority=[";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>"],
                                     ShadowHashData=[b"usable password material"])
        self.swap_args.disable_legacy_build_account = True
        self.swap(manifest)
        user = plistlib.loads(account.read_bytes())
        self.assertEqual(user["authentication_authority"], [";DisabledUser;"])
        self.assertNotIn("ShadowHashData", user)
        assert self.guest_data is not None
        admin = plistlib.loads((self.guest_data / "private/var/db/dslocal/nodes/Default/groups/admin.plist").read_bytes())
        self.assertNotIn("motorbuild", admin.get("users", []))
        self.assertFalse(set(user.get("generateduid", [])) & set(admin.get("groupmembers", [])))
        self.assertTrue(self.swap_args.destination.exists())

    def test_build_account_lock_checks_reject_ambiguous_credentials_and_any_ssh_files(self):
        self.swap_inputs()
        assert self.guest_data is not None
        for changes, reason in (({"shell": ["/bin/zsh"]}, "name/shell"),
                                ({"passwd": ["********"]}, "passwd"),
                                ({"ShadowHashData": [b"hash"]}, "ShadowHashData remains"),
                                ({"authentication_authority": [";DisabledUser;", ";Kerberosv5;method"]}, "enabled or unknown"),
                                ({"authentication_authority": ";DisabledUser;"}, "malformed"),
                                ({"home": ["/Users/elsewhere"]}, "home must be")):
            with self.subTest(changes=changes):
                self.build_account(**changes)
                with self.assertRaisesRegex(ValueError, reason):
                    image.require_locked_build_account(self.guest_data)
        account = self.build_account()
        image.require_locked_build_account(self.guest_data)
        ssh = self.guest_data / "Users/motorbuild/.ssh"
        ssh.mkdir(parents=True)
        for key in ("authorized_keys", "authorized_keys2", "id_ed25519", "config"):
            with self.subTest(key=key):
                (ssh / key).write_text("unreviewed SSH material")
                with self.assertRaisesRegex(ValueError, "SSH configuration"):
                    image.require_locked_build_account(self.guest_data)
                (ssh / key).unlink()
        ssh.rmdir()
        ssh.symlink_to(account)
        with self.assertRaisesRegex(ValueError, "symlinks"):
            image.require_locked_build_account(self.guest_data)

    def test_agent_swap_refuses_existing_destination_without_modifying_it(self):
        manifest = self.swap_inputs()
        self.swap_args.destination.mkdir()
        sentinel = self.swap_args.destination / "keep"
        sentinel.write_bytes(b"existing destination")
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.swap(manifest)
        self.assertEqual(sentinel.read_bytes(), b"existing destination")
        self.assertFalse(any(args[0] == "/bin/cp" for args in self.commands))

    def test_agent_swap_never_stages_inside_the_source_even_when_running_as_root(self):
        manifest = self.swap_inputs()
        self.swap_args.destination = self.bundle / "nested"
        before = sorted(path.name for path in self.bundle.iterdir())
        with self.assertRaisesRegex(ValueError, "outside the sealed source"):
            self.swap(manifest)
        self.assertEqual(sorted(path.name for path in self.bundle.iterdir()), before)


class GuestServiceTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="motor-job-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.policy = self.root / "policy.txt"
        self.policy.write_text("unverified com.apple.analyticsd # No analytics upload in job VMs.\n")
        self.daemons = self.root / "LaunchDaemons"
        self.daemons.mkdir()
        (self.daemons / "analytics.plist").write_bytes(plistlib.dumps({"Label": "com.apple.analyticsd"}))

    def apply(self, result=None):
        result = result or subprocess.CompletedProcess([], 0, "", "")
        with patch.object(guest_services.subprocess, "run", return_value=result) as run, \
                patch.object(guest_services.subprocess, "check_output", return_value=DATA_UUID):
            receipt = guest_services.apply(self.root, self.policy, self.daemons)
        return receipt, run

    def verify(self, disabled='disabled services = {\n "com.apple.analyticsd" => true\n}',
               spotlight="/:\n\tIndexing disabled.\n", pid=0, boot=NEXT_BOOT):
        system = f"system = {{\n services = {{\n {pid} - com.apple.analyticsd\n }}\n}}"
        with patch.object(guest_services.subprocess, "check_output", side_effect=[boot, disabled, system, spotlight]) as output:
            receipt = guest_services.verify(self.root, self.policy)
        return receipt, output

    def test_policy_parsing_requires_status_label_and_reason(self):
        self.assertEqual(guest_services.parse("# heading\n\noff com.apple.analyticsd # No uploads\n"
                                             "kept-sip com.apple.locationd # Protected\n"),
                         {"com.apple.analyticsd": "off", "com.apple.locationd": "kept-sip"})
        for text in ("", "# comments only", "com.apple.analyticsd # Missing outcome",
                     "off system/com.apple.analyticsd # Wrong domain", "off com.apple.analyticsd",
                     "maybe com.apple.analyticsd # Unknown status", "off com.apple.* # No wildcards",
                     "off com.apple.analyticsd # first\nunverified com.apple.analyticsd # duplicate"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                guest_services.parse(text)

    def test_pinned_policy_keeps_essential_services(self):
        services = guest_services.parse(guest_services.POLICY.read_text())
        self.assertTrue({"com.apple.metadata.mds", "com.apple.metadata.mds.index", "com.apple.analyticsd",
                         "com.apple.mobileassetd", "com.apple.airportd"}.issubset(services))
        for label in ("io.cortexkit.motor.macagent", "com.apple.logd", "com.apple.opendirectoryd",
                      "com.apple.trustd", "com.apple.syspolicyd", "com.apple.tccd", "com.apple.mDNSResponder",
                      "com.apple.nehelper", "com.apple.configd", "com.apple.diskarbitrationd", "com.apple.fskitd",
                      "com.apple.kernelmanagerd", "com.apple.runningboardd", "com.apple.lsd", "com.apple.UserEventAgent-System",
                      "com.apple.notifyd", "com.apple.securityd", "com.apple.ReportCrash", "com.apple.spindump"):
            self.assertNotIn(label, services)

    def test_provisioning_uses_system_domain_and_all_mounted_spotlight_volumes_idempotently(self):
        first, run = self.apply()
        self.assertEqual([call.args for call in run.call_args_list], [
            (["/bin/launchctl", "disable", "system/com.apple.analyticsd"],),
            (["/usr/bin/mdutil", "-a", "-i", "off"],),
        ])
        self.assertEqual(run.call_args_list[0].kwargs, {"check": True})
        self.assertEqual(run.call_args_list[1].kwargs, {"check": True})
        before = (self.root / guest_services.POLICY.name).read_text()
        second, _ = self.apply()
        self.assertEqual(first, second)
        self.assertEqual(before, (self.root / guest_services.POLICY.name).read_text())
        self.assertEqual(first["results"], {})
        self.assertEqual(first["provision_boot_session"], DATA_UUID)

    def test_nonexistent_label_fails_instead_of_creating_a_meaningless_override(self):
        self.policy.write_text("unverified com.apple.misspelled # Not a real service\n")
        with patch.object(guest_services.subprocess, "run") as run, self.assertRaisesRegex(ValueError, "absent"):
            guest_services.apply(self.root, self.policy, self.daemons)
        run.assert_not_called()

    def test_still_running_after_reboot_is_recorded_as_kept_sip(self):
        self.apply()
        for disabled in ('"com.apple.analyticsd" => true', '"com.apple.analyticsd" => false'):
            with self.subTest(disabled=disabled):
                actual, _ = self.verify(disabled=disabled, pid=123)
                self.assertEqual(actual["results"]["com.apple.analyticsd"], {
                    "outcome": "kept-sip", "disabled": disabled.endswith("true"), "pid": 123})
                self.assertIn('"outcome": "kept-sip"', (self.root / guest_services.POLICY.name).read_text())

    def test_disable_command_errors_fail_instead_of_becoming_kept_sip(self):
        with patch.object(guest_services.subprocess, "run", side_effect=subprocess.CalledProcessError(1, ["launchctl"])), \
                self.assertRaises(subprocess.CalledProcessError):
            guest_services.apply(self.root, self.policy, self.daemons)
        self.assertFalse((self.root / guest_services.POLICY.name).exists())

    def test_pinned_off_service_running_after_reboot_is_a_regression(self):
        self.policy.write_text("off com.apple.analyticsd # No uploads\n")
        self.apply()
        with self.assertRaisesRegex(ValueError, "still running"):
            self.verify(pid=123)

    def test_seal_requires_a_new_boot(self):
        self.apply()
        with self.assertRaisesRegex(ValueError, "must reboot"):
            self.verify(boot=DATA_UUID)

    def test_pinned_sip_services_are_not_touched(self):
        self.policy.write_text("kept-sip com.apple.analyticsd # Protected\n")
        receipt, run = self.apply()
        self.assertEqual([call.args for call in run.call_args_list], [(["/usr/bin/mdutil", "-a", "-i", "off"],)])
        self.assertEqual(receipt["results"], {})
        actual, _ = self.verify(disabled="{}", pid=123)
        self.assertEqual(actual["results"]["com.apple.analyticsd"], {"outcome": "kept-sip", "disabled": False, "pid": 123})

    def test_mdutil_failure_does_not_write_success_receipt(self):
        with patch.object(guest_services.subprocess, "run", side_effect=[
                subprocess.CompletedProcess([], 0, "", ""), subprocess.CalledProcessError(1, ["mdutil"])]), \
                self.assertRaises(subprocess.CalledProcessError):
            guest_services.apply(self.root, self.policy, self.daemons)
        self.assertFalse((self.root / guest_services.POLICY.name).exists())

    def test_seal_checks_actual_disabled_overrides_and_root_spotlight(self):
        receipt, _ = self.apply()
        actual, output = self.verify()
        self.assertEqual(actual["policy_sha256"], receipt["policy_sha256"])
        self.assertEqual(actual["results"], {"com.apple.analyticsd": {"outcome": "off", "disabled": True, "pid": 0}})
        self.assertEqual([call.args for call in output.call_args_list], [
            (["/usr/sbin/sysctl", "-n", "kern.bootsessionuuid"],),
            (["/bin/launchctl", "print-disabled", "system"],), (["/bin/launchctl", "print", "system"],),
            (["/usr/bin/mdutil", "-s", "/"],),
        ])
        for disabled in ('"com.apple.analyticsd" => false', '"com.apple.analyticsd.extra" => true',
                         '"comXappleXanalyticsd" => true', "disabled services = {}"):
            with self.subTest(disabled=disabled), self.assertRaisesRegex(ValueError, "not disabled"):
                self.verify(disabled=disabled)
        for status in ("/:\n Indexing enabled.\n", "", "/Volumes/Other:\n Indexing disabled.\n", "Spotlight server is disabled."):
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "Spotlight"):
                self.verify(spotlight=status)

    def test_seal_rejects_missing_changed_and_unresolved_results(self):
        with self.assertRaises(FileNotFoundError):
            self.verify()
        self.apply()
        receipt, _ = self.verify()
        path = self.root / guest_services.POLICY.name
        original = path.read_text()
        for installed in (self.policy.read_text(), original.replace("No analytics upload", "Changed policy")):
            path.write_text(installed)
            with self.assertRaisesRegex(ValueError, "missing or changed"):
                self.verify()
        for change in (lambda value: value.update(policy_sha256="0" * 64),
                       lambda value: value["results"].clear(),
                       lambda value: value["results"].update({"com.apple.analyticsd": {"outcome": "unverified"}}),
                       lambda value: value["results"].update({"com.apple.analyticsd": {"outcome": "kept-sip", "evidence": "any error"}})):
            bad = copy.deepcopy(receipt)
            change(bad)
            path.write_text(self.policy.read_text() + guest_services.RESULTS + json.dumps(bad) + "\n")
            with self.subTest(receipt=bad), self.assertRaises(ValueError):
                guest_services.validate_receipt(bad, self.policy.read_text())

    def test_missing_service_table_or_command_failure_never_counts_as_stopped(self):
        self.apply()
        for table in ("", "system = {}", "system = {\n services = {\n malformed row\n }\n}"):
            with self.subTest(table=table), patch.object(guest_services.subprocess, "check_output",
                    side_effect=[NEXT_BOOT, '"com.apple.analyticsd" => true', table]), self.assertRaises(ValueError):
                guest_services.verify(self.root, self.policy)
        with patch.object(guest_services.subprocess, "check_output", side_effect=subprocess.CalledProcessError(1, ["launchctl"])), \
                self.assertRaises(subprocess.CalledProcessError):
            guest_services.verify(self.root, self.policy)

    def test_running_services_reads_pids_not_last_exit_status_from_system_table(self):
        table = """system = {
	services = {
		     170      - 	com.apple.runningboardd
		       0      - 	com.apple.diagnosticextensions.osx.spotlight.helper
		       0      0 	com.apple.AmbientDisplayAgent
	}
}"""
        self.assertEqual(guest_services.running_services(table), {
            "com.apple.runningboardd": 170, "com.apple.diagnosticextensions.osx.spotlight.helper": 0,
            "com.apple.AmbientDisplayAgent": 0})

    def test_failed_service_seal_keeps_build_access_and_never_shuts_down(self):
        manifest = ready()
        with patch.object(guest.pinlock, "require_ready"), patch.object(guest.parity, "digest", return_value="a" * 64), \
                patch.object(guest, "clt_verify"), patch.object(guest, "PREFIX", self.root), \
                patch.object(guest.parity, "capture"), patch.object(guest.parity, "differences", return_value=[]), \
                patch.object(guest_services, "verify", side_effect=ValueError("service is not disabled")), \
                patch.object(guest, "drop_build_access") as drop, patch.object(guest, "run") as run:
            with self.assertRaisesRegex(ValueError, "not disabled"):
                guest.seal(manifest, "motorbuild", self.root / "scratch")
        drop.assert_not_called()
        run.assert_not_called()

    def test_successful_guest_seal_persists_and_reports_service_outcomes(self):
        manifest = ready()
        receipt = service_receipt()
        scratch = self.root / "scratch"
        scratch.mkdir()
        with patch.object(guest.pinlock, "require_ready"), patch.object(guest.parity, "digest", return_value="a" * 64), \
                patch.object(guest, "clt_verify"), patch.object(guest, "PREFIX", self.root), \
                patch.object(guest.parity, "capture"), patch.object(guest.parity, "differences", return_value=[]), \
                patch.object(guest_services, "verify", return_value=receipt), \
                patch("pwd.getpwnam", return_value=SimpleNamespace(pw_dir="/Users/motorbuild")), \
                patch.object(guest, "drop_build_access"), patch.object(guest, "run") as run, \
                patch("sys.stdout", new_callable=io.StringIO) as output:
            guest.seal(manifest, "motorbuild", scratch)
        self.assertEqual(json.loads((self.root / "sealed.json").read_text())["guest_services"], receipt)
        self.assertIn("MOTOR_GUEST_SERVICES " + json.dumps(receipt, sort_keys=True), output.getvalue())
        self.assertFalse(scratch.exists())
        run.assert_called_once_with("/sbin/shutdown", "-h", "now")

    def test_provision_uploads_policy_and_reboots_after_collecting_outputs(self):
        args = SimpleNamespace(work=self.root, guest="motorbuild@guest", known_hosts=self.root / "known_hosts",
                               measure=True, clt_label=None)
        calls = []

        def reboot(*unused):
            self.assertIn("sudo -n tar -cf", calls[-1][-1])

        with patch.object(image, "prepare_inputs", return_value=self.root / "inputs"), \
                patch.object(image, "run", side_effect=lambda *args, **kwargs: calls.append(args)), \
                patch.object(image, "reboot_for_service_policy", side_effect=reboot) as restart:
            image.provision(args, ready())
        restart.assert_called_once()
        for name in ("guest_services.py", "guest-services-off.txt"):
            self.assertEqual((self.root / "recipe" / name).read_bytes(), (HERE / name).read_bytes())

    def test_reboot_waits_for_a_new_guest_boot_not_just_working_ssh(self):
        args = SimpleNamespace(guest="motorbuild@guest")
        results = [subprocess.CompletedProcess([], 255, "", "connection closed"),
                   subprocess.CompletedProcess([], 0, DATA_UUID + "\n", ""),
                   subprocess.CompletedProcess([], 255, "", "not ready"),
                   subprocess.CompletedProcess([], 0, NEXT_BOOT + "\n", "")]
        with patch.object(image.subprocess, "check_output", return_value=DATA_UUID), \
                patch.object(image.subprocess, "run", side_effect=results) as run, patch.object(image.time, "sleep") as sleep:
            image.reboot_for_service_policy(args, [])
        self.assertEqual(run.call_count, 4)
        self.assertEqual(sleep.call_count, 2)
        self.assertEqual(run.call_args_list[0].args[0][-1], "sudo -n /sbin/shutdown -r now")

    def test_reboot_timeout_fails_instead_of_sealing_the_old_boot(self):
        with patch.object(image.subprocess, "check_output", return_value=DATA_UUID), \
                patch.object(image.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")), \
                patch.object(image.time, "monotonic", side_effect=[0, 301]), \
                self.assertRaisesRegex(ValueError, "did not return on a new boot"):
            image.reboot_for_service_policy(SimpleNamespace(guest="motorbuild@guest"), [])

    def test_host_seal_uploads_policy_and_requires_service_outcomes_before_deleting_keys(self):
        manifest = ready()
        path = self.root / "toolchain.json"
        path.write_text(json.dumps(manifest))
        (self.root / "candidate.json").write_text(json.dumps(dict(manifest, release_status="pending review")))
        args = SimpleNamespace(work=self.root, manifest=path, guest="motorbuild@guest", known_hosts=self.root / "known_hosts")
        marker = "MOTOR_SEALED " + parity.digest(path) + "\n"
        receipt = service_receipt()
        for outcomes in ("", "MOTOR_GUEST_SERVICES {}\n", "MOTOR_GUEST_SERVICES " + json.dumps(receipt) + "\n"):
            for name in ("build_key", "build_key.pub"):
                (self.root / name).write_text("test-only key fixture")
            response = subprocess.CompletedProcess([], 0, outcomes + marker, "")
            with self.subTest(outcomes=outcomes), patch.object(image, "require_committed"), \
                    patch.object(image, "run") as run, patch.object(image.subprocess, "run", return_value=response):
                if outcomes in ("", "MOTOR_GUEST_SERVICES {}\n"):
                    with self.assertRaises(ValueError):
                        image.seal(args, manifest)
                    self.assertTrue((self.root / "build_key").exists())
                else:
                    image.seal(args, manifest)
                    self.assertFalse((self.root / "build_key").exists())
                    self.assertEqual(json.loads((self.root / "guest-sealed.json").read_text())["guest_services"], receipt)
                uploaded = run.call_args_list[-1].args
                self.assertIn(HERE / "guest_services.py", uploaded)
                self.assertIn(HERE / "guest-services-off.txt", uploaded)


class ImageTests(unittest.TestCase):
    def seal_fixture(self, directory):
        home = Path(directory)
        (home / ".ssh").mkdir()
        (home / ".ssh/authorized_keys").write_text("build-only public key")
        sudoers = home / "sudoers"
        sudoers.write_text("temporary bootstrap permission")
        return home, sudoers

    def not_admin(self):
        return patch.object(guest.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", ""))

    def test_sealing_leaves_admin_then_disables_remote_login_then_access(self):
        with tempfile.TemporaryDirectory() as directory:
            home, sudoers = self.seal_fixture(directory)
            with patch.object(guest, "run") as run, self.not_admin(), \
                    patch.object(guest.subprocess, "check_output", return_value="Remote Login: Off"):
                guest.drop_build_access("motorbuild", home, sudoers)
            self.assertFalse((home / ".ssh").exists())
            self.assertFalse(sudoers.exists())
            calls = [call.args for call in run.call_args_list]
            self.assertEqual(calls, [
                ("/usr/sbin/dseditgroup", "-o", "edit", "-d", "motorbuild", "-t", "user", "admin"),
                ("/usr/sbin/systemsetup", "-f", "-setremotelogin", "off"),
            ])
            # The Secure Token holder's record is never edited online: macOS refuses
            # it, and a non-login shell would end the SSH route before the seal ends.
            self.assertFalse(any(call[0] == "/usr/bin/dscl" for call in calls))

    def test_sealing_keeps_build_access_while_the_account_is_still_admin(self):
        with tempfile.TemporaryDirectory() as directory:
            home, sudoers = self.seal_fixture(directory)
            member = subprocess.CompletedProcess([], 0, "yes motorbuild is a member of admin", "")
            with patch.object(guest, "run") as run, patch.object(guest.subprocess, "run", return_value=member):
                with self.assertRaisesRegex(ValueError, "still an admin member"):
                    guest.drop_build_access("motorbuild", home, sudoers)
            self.assertTrue(sudoers.exists())
            self.assertTrue((home / ".ssh/authorized_keys").exists())
            self.assertNotIn(("/usr/sbin/systemsetup", "-f", "-setremotelogin", "off"),
                             [call.args for call in run.call_args_list])

    def test_sealing_refuses_if_remote_login_stays_on(self):
        with tempfile.TemporaryDirectory() as directory:
            home, sudoers = self.seal_fixture(directory)
            with patch.object(guest, "run"), self.not_admin(), \
                    patch.object(guest.subprocess, "check_output", return_value="Remote Login: On"):
                with self.assertRaisesRegex(ValueError, "did not turn off"):
                    guest.drop_build_access("motorbuild", home, sudoers)
            # The SSH route stays usable, so the builder can be inspected.
            self.assertTrue((home / ".ssh/authorized_keys").exists())
            self.assertTrue(sudoers.exists())

    def test_sealing_keeps_build_access_when_remote_login_cannot_be_changed(self):
        with tempfile.TemporaryDirectory() as directory:
            home, sudoers = self.seal_fixture(directory)
            refused = subprocess.CalledProcessError(1, ["/usr/sbin/systemsetup"])

            def run(*args, **kwargs):
                if args[0] == "/usr/sbin/systemsetup":
                    raise refused

            with patch.object(guest, "run", side_effect=run), self.not_admin():
                with self.assertRaises(subprocess.CalledProcessError):
                    guest.drop_build_access("motorbuild", home, sudoers)
            self.assertTrue((home / ".ssh/authorized_keys").exists())
            self.assertTrue(sudoers.exists())

    def test_ipsw_digest_and_size_are_both_enforced(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "image.ipsw"
            path.write_bytes(b"pinned restore")
            pin = {"size": 14, "sha256": parity.digest(path), "build": "26A434"}
            image.verify_ipsw(path, pin)
            for field, value in (("size", 15), ("sha256", "0" * 64)):
                with self.subTest(field=field), self.assertRaisesRegex(ValueError, "size/SHA-256 differs"):
                    image.verify_ipsw(path, dict(pin, **{field: value}))

    def test_candidates_are_never_automatically_reviewed(self):
        manifest = ready()
        with tempfile.TemporaryDirectory(prefix="motor-job-") as directory:
            actual = observed(manifest, Path(directory))
            result = pinlock.candidate(manifest, actual, {}, "b" * 64, {"cargo": "c" * 64})
        self.assertEqual(result["release_status"], "pending review")
        with self.assertRaisesRegex(ValueError, "not reviewed"):
            pinlock.require_ready(result)

    def test_github_download_uses_ranges_and_hash_check(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "bun.zip"
            def ranges(url, path):
                path.write_bytes(b"bun")
            with patch.object(download, "parallel_ranges", side_effect=ranges) as ranges:
                with self.assertRaisesRegex(ValueError, "SHA-256 differs"):
                    download.fetch("https://github.com/oven-sh/bun/release", destination, "0" * 64)
                self.assertFalse(destination.exists())
                self.assertFalse(destination.with_name("bun.zip.download").exists())
                ranges.assert_called_once()

    def test_pending_download_needs_explicit_measurement(self):
        with patch.object(download.urllib.request, "urlopen") as request:
            with self.assertRaisesRegex(ValueError, "no reviewed SHA-256"):
                download.fetch("https://example.com/bun", Path("unused"), pinlock.PENDING)
            request.assert_not_called()

    def test_parallel_download_requires_exact_ranges_and_reassembles_bytes(self):
        payload = b"a distinct payload split across six ranges"
        requests = []

        class Response(io.BytesIO):
            def __init__(self, body, status, headers):
                super().__init__(body)
                self.status, self.headers = status, headers

        def request(value, **kwargs):
            requests.append(value)
            if value.get_method() == "HEAD":
                return Response(b"", 200, {"Content-Length": str(len(payload))})
            start, end = map(int, value.get_header("Range").removeprefix("bytes=").split("-"))
            return Response(payload[start:end + 1], 206, {"Content-Range": f"bytes {start}-{end}/{len(payload)}"})

        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "archive"
            with patch.object(download.urllib.request, "urlopen", side_effect=request):
                download.parallel_ranges("https://github.com/example/archive", destination)
            self.assertEqual(destination.read_bytes(), payload)
            self.assertEqual(len([item for item in requests if item.get_method() == "GET"]), 6)
            self.assertEqual(list(Path(directory).iterdir()), [destination])

    def test_server_without_ranges_falls_back_to_one_hash_checked_stream(self):
        payload = b"whole archive"

        class Response(io.BytesIO):
            status = 200
            headers = {"Content-Length": str(len(payload))}
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(download.urllib.request, "urlopen", side_effect=lambda *args, **kwargs: Response(payload)):
                with self.assertRaises(download.RangeUnsupported):
                    download.parallel_ranges("https://github.com/example/archive", Path(directory) / "part")
                destination = Path(directory) / "archive"
                expected = hashlib.sha256(payload).hexdigest()
                self.assertEqual(download.fetch("https://github.com/example/archive", destination, expected), expected)
                self.assertEqual(destination.read_bytes(), payload)
                with self.assertRaisesRegex(ValueError, "SHA-256 differs"):
                    download.fetch("https://github.com/example/archive", Path(directory) / "bad", "0" * 64)
            self.assertEqual(sorted(path.name for path in Path(directory).iterdir()), ["archive"])

    def test_all_image_seal_entrypoints_reject_pending_lock(self):
        manifest = pending()
        for operation in (image.seal, image.finalize, image.agent_swap):
            with self.subTest(operation=operation.__name__), self.assertRaisesRegex(ValueError, "not reviewed"):
                operation(None, manifest)
        with self.assertRaisesRegex(ValueError, "not reviewed"):
            install.install(None, manifest)

if __name__ == "__main__":
    unittest.main(verbosity=2)
