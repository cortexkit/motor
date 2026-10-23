#!/usr/bin/env python3
"""Portable contract tests for the macOS image, installer and parity boundary."""

import copy
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
import tempfile
import unittest
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import download
import guest
import image
import install
import parity
import pinlock

SERVER = json.loads((HERE.parent / "runner/toolchain.json").read_text())


def pending():
    return pinlock.seed(SERVER)


def ready():
    manifest = pending()

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
        self.assertEqual(manifest["ipsw"]["build"], "26A434")
        self.assertEqual(manifest["ipsw"]["size"], 26637307067)
        self.assertEqual(manifest["ipsw"]["sha256"], "2f016638293c3e641b8b25391a76fbc16563b3711915a5551cf8aa0f5598a5c1")
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

    def test_build_server_rustc_drift_is_rejected_before_capture(self):
        server = copy.deepcopy(SERVER)
        server["rust_toolchains"]["1.99.0-x86_64-unknown-linux-gnu"]["rustc"] = "rustc 1.99.0 (deadbeef 2026-09-28)"
        with self.assertRaisesRegex(ValueError, "differ from build-server"):
            parity.validate(self.manifest, server)

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


class InstallerTests(unittest.TestCase):
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
        for name in ("vault-ca.pub", "principals"):
            (self.ssh / name).write_text(name)
        self.sshd = self.root / "sshd_config"
        self.sshd.write_text("Port 22\n" + install.ssh_block(self.ssh / "vault-ca.pub", self.ssh / "principals"))
        installed = {"manifest_sha256": parity.digest(self.config / "toolchain.json"),
                     "helper": str(self.config / "motor-macvm"), "runner": str(self.config / "motor-runner"),
                     "vault_ca_sha256": parity.digest(self.ssh / "vault-ca.pub"),
                     "principals_sha256": parity.digest(self.ssh / "principals")}
        (self.config / "install.json").write_text(json.dumps(installed))
        (self.config / "vm.toml").write_text(install.vm_configuration(self.image, Path(installed["helper"]), self.state / "tools", self.manifest))
        self.plist = self.root / "daemon.plist"
        self.plist.write_bytes(plistlib.dumps(install.daemon(Path(installed["runner"]), self.state, "macos", self.config / "vm.toml")))
        files = {}
        for name in (*image.IMAGE_FILES, "agent.sha256"):
            (self.image / name).write_bytes(name.encode())
            files[name] = parity.digest(self.image / name)
        (self.image / "image.json").write_text(json.dumps({"manifest_sha256": installed["manifest_sha256"], "files": files}))
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
        self.patches = [patch.object(install, "internal_volume", return_value=True),
                        patch.object(install, "entitlement", return_value=True),
                        patch.object(install, "tool_errors", return_value=[]),
                        patch.object(install.shutil, "disk_usage", return_value=shutil._ntuple_diskusage(100, 50, install.FLOOR)),
                        patch.object(install.pwd, "getpwnam", return_value=type("Account", (), {"pw_uid": os.getuid()})),
                        patch.object(install, "run", return_value=f"trustedusercakeys {self.ssh}/vault-ca.pub\nauthorizedprincipalsfile {self.ssh}/principals\nauthorizedkeysfile none")]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)

    def check(self):
        return install.check_install(self.state, self.image, "macos", self.manifest, self.config, self.plist, self.sshd, self.ssh)

    def test_valid_installed_fixture_passes(self):
        self.assertEqual(self.check(), [])

    def test_external_state_and_external_image_are_each_refused(self):
        for external in (self.state, self.image):
            with self.subTest(path=external), patch.object(install, "internal_volume", side_effect=lambda path: path != external):
                self.assertIn("not on an internal", " ".join(self.check()))

    def test_missing_virtualization_entitlement_is_refused(self):
        with patch.object(install, "entitlement", return_value=False):
            self.assertIn("lacks virtualization entitlement", " ".join(self.check()))

    def test_untrusted_effective_sshd_ca_is_refused(self):
        with patch.object(install, "run", return_value="trustedusercakeys none\nauthorizedprincipalsfile none\nauthorizedkeysfile none"):
            self.assertIn("does not trust the vault CA", " ".join(self.check()))

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
        ca = self.ssh / "vault-ca-source.pub"
        ca.write_text("ssh-ed25519 AAAA vault\n")
        args = SimpleNamespace(state_root=self.state, image=self.image, tool_bundle=bundle, manifest=manifest_path,
                               runner=runner, vm_helper=helper, vault_ca=ca, instance="macos")
        account = SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())
        with patch.multiple(install, CONFIG=self.config, PLIST=self.plist, SSHD=self.sshd, SSH_ROOT=self.ssh), \
                patch.object(install, "create_account", return_value=account), patch.object(install.os, "chown"), \
                patch.object(install, "run") as run, patch.object(install.subprocess, "run"):
            install.install(args, self.manifest)
            install.install(args, self.manifest)
        signed = [call for call in run.call_args_list if call.args[:2] == ("/usr/bin/codesign", "--force")]
        self.assertEqual(len(signed), 2)
        for call in signed:
            self.assertEqual(call.args, ("/usr/bin/codesign", "--force", "--sign", "-", "--entitlements",
                                         HERE / "virtualization.entitlements", self.config / "motor-macvm"))
        self.assertEqual(install.plist_errors(plistlib.loads(self.plist.read_bytes())), [])

    def test_runtime_config_cannot_select_an_unchecked_helper_or_tool(self):
        path = self.config / "vm.toml"
        path.write_text(path.read_text().replace("motor-macvm", "unsigned-macvm"))
        self.assertIn("VM configuration differs from installed pins", self.check())


class ImageTests(unittest.TestCase):
    def test_sealing_disables_remote_login_and_deletes_build_access(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / ".ssh").mkdir()
            (home / ".ssh/authorized_keys").write_text("build-only public key")
            sudoers = home / "sudoers"
            sudoers.write_text("temporary bootstrap permission")
            with patch.object(guest, "run") as run, patch.object(guest.subprocess, "check_output", return_value="Remote Login: Off"):
                guest.drop_build_access("motorbuild", home, sudoers)
            self.assertFalse((home / ".ssh").exists())
            self.assertFalse(sudoers.exists())
            run.assert_any_call("/usr/sbin/systemsetup", "-f", "-setremotelogin", "off")
            run.assert_any_call("/usr/sbin/dscl", ".", "-create", "/Users/motorbuild", "UserShell", "/usr/bin/false")

    def test_sealing_refuses_if_remote_login_stays_on(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(guest, "run"), patch.object(guest.subprocess, "check_output", return_value="Remote Login: On"):
                with self.assertRaisesRegex(ValueError, "did not turn off"):
                    guest.drop_build_access("motorbuild", Path(directory), Path(directory) / "sudoers")

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

    def test_range_server_cannot_fall_back_to_serial_download(self):
        class Response(io.BytesIO):
            status = 200
            headers = {"Content-Length": "12"}
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(download.urllib.request, "urlopen", side_effect=lambda *args, **kwargs: Response(b"not a range")):
                with self.assertRaisesRegex(ValueError, "ignored range request"):
                    download.parallel_ranges("https://github.com/example/archive", Path(directory) / "archive")
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_all_image_seal_entrypoints_reject_pending_lock(self):
        manifest = pending()
        for operation in (image.seal, image.finalize):
            with self.subTest(operation=operation.__name__), self.assertRaisesRegex(ValueError, "not reviewed"):
                operation(None, manifest)
        with self.assertRaisesRegex(ValueError, "not reviewed"):
            install.install(None, manifest)

if __name__ == "__main__":
    unittest.main(verbosity=2)
