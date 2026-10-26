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
        (self.ssh / "authorized_keys").write_text("ssh-ed25519 AAAA\n")
        self.sshd = self.root / "sshd_config"
        self.sshd.write_text("Port 22\n" + install.ssh_block(self.ssh / "authorized_keys"))
        installed = {"manifest_sha256": parity.digest(self.config / "toolchain.json"),
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
        self.patches = [patch.object(install, "internal_volume", return_value=True),
                        patch.object(install, "entitlement", return_value=True),
                        patch.object(install, "tool_errors", return_value=[]),
                        patch.object(install.shutil, "disk_usage", return_value=shutil._ntuple_diskusage(100, 50, install.FLOOR)),
                        patch.object(install.pwd, "getpwnam", return_value=type("Account", (), {"pw_uid": os.getuid()})),
                         patch.object(install, "LEGACY_PLIST", self.root / "legacy.plist"),
                         patch.object(image, "mounted_data", return_value=nullcontext(self.image)),
                         patch.object(image, "agent_requirement", return_value=self.requirement),
                         patch.object(install.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "no-timeout")),
                         patch.object(install, "run", side_effect=self.command)]
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

    def test_no_aqua_session_reports_restart_even_without_socket(self):
        self.socket_path.unlink()
        with patch.object(install, "has_gui_session", return_value=False):
            errors = " ".join(self.check())
        self.assertIn("no Aqua login session", errors)
        self.assertIn("restart for auto-login", errors)
        self.assertIn("runner socket is missing", errors)

    def test_wrong_auto_login_user_and_filevault_are_refused(self):
        with patch.object(install, "auto_login_user", return_value="rentamac"):
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
        for effective, expected in cases.items():
            with self.subTest(effective=effective), patch.object(install, "run", side_effect=lambda *args: effective if args[:2] == ("/usr/sbin/sshd", "-T") else self.command(*args)):
                self.assertIn(expected, " ".join(self.check()))

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
                               runner=runner, vm_helper=helper, runner_key=key, instance="macos")
        account = SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid())
        install.LEGACY_PLIST.touch()
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

    def test_runtime_config_cannot_select_an_unchecked_helper_or_tool(self):
        path = self.config / "vm.toml"
        path.write_text(path.read_text().replace("motor-macvm", "unsigned-macvm"))
        self.assertIn("VM configuration differs from installed pins", self.check())


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
        self.user = "rentamac"
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
        if args[:2] == ("/usr/bin/defaults", "export"):
            values = {} if self.user is None else {"autoLoginUser": self.user}
            return plistlib.dumps(values).decode()
        if args[:2] == ("/usr/bin/defaults", "write"):
            self.user = args[-1]
        elif args[:2] == ("/usr/bin/defaults", "delete"):
            self.user = None
        elif args[:5] == install.as_motor("/usr/bin/security"):
            if args[5:] == ("set-keychain-settings", self.keychain):
                return ""
            self.assertEqual(args[5:8], ("default-keychain", "-d", "user"))
            if len(args) == 8:
                return '"' + self.default_keychain + '"'
            self.assertEqual(args[8], "-s")
            self.default_keychain = str(args[9]) if len(args) == 10 else ""
        elif args[:2] == ("/usr/sbin/sshd", "-t"):
            pass
        else:
            self.fail("unexpected command: " + str(args))
        return ""

    def native_command(self, args, **kwargs):
        if args[:4] == ["/usr/bin/dscl", ".", "-passwd", "/Users/motor"]:
            self.assertTrue(args[4])
        elif args[:6] == list(install.as_motor("/usr/bin/security", "create-keychain")):
            self.assertEqual(args[6], "-p")
            Path(args[-1]).write_bytes(b"new keychain")
        elif args[:6] == list(install.as_motor("/usr/bin/security", "unlock-keychain")):
            self.assertEqual(args[6:], ["-p", self.password.read_text().strip(), str(self.keychain)])
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
        install.uninstall()
        self.assertEqual(self.user, "rentamac")
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
        self.assertEqual(self.user, "rentamac")

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
        self.assertEqual(self.default_keychain, "")

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
        (self.root / "guest-sealed.json").write_text(json.dumps({"manifest_sha256": parity.digest(path), "remote_login": "Off"}))
        args = SimpleNamespace(work=self.root, image=self.bundle, manifest=path)
        with patch.object(image, "require_committed"), patch("sys.stdout", io.StringIO()):
            image.finalize(args, manifest)

    def test_finalize_hashes_the_granted_disk_and_records_requirement(self):
        self.finalize()
        pin = json.loads((self.bundle / "image.json").read_text())
        self.assertEqual(pin["files"]["disk.img"], hashlib.sha256(b"granted disk").hexdigest())
        self.assertEqual(pin["tcc"], {"services": ["kTCCServiceSystemPolicyNetworkVolumes", "kTCCServiceSystemPolicyAllFiles"],
                                     "requirement": self.requirement})
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


class ImageTests(unittest.TestCase):
    def test_sealing_disables_remote_login_and_deletes_build_access(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / ".ssh").mkdir()
            (home / ".ssh/authorized_keys").write_text("build-only public key")
            sudoers = home / "sudoers"
            sudoers.write_text("temporary bootstrap permission")
            with patch.object(guest, "run") as run, patch.object(guest.subprocess, "check_output", side_effect=["Remote Login: Off", "root\nmotorjob\n"]):
                guest.drop_build_access("motorbuild", home, sudoers)
            self.assertFalse((home / ".ssh").exists())
            self.assertFalse(sudoers.exists())
            run.assert_any_call("/usr/sbin/systemsetup", "-f", "-setremotelogin", "off")
            run.assert_any_call("/usr/bin/dscl", ".", "-create", "/Users/motorbuild", "UserShell", "/usr/bin/false")
            run.assert_any_call("/usr/sbin/dseditgroup", "-o", "edit", "-d", "motorbuild", "-t", "user", "admin")
            run.assert_any_call("/usr/bin/dscl", ".", "-delete", "/Users/motorbuild")

    def test_sealing_refuses_if_build_account_survives_deletion(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(guest, "run"), patch.object(guest.subprocess, "check_output", side_effect=["Remote Login: Off", "root\nmotorbuild\n"]):
                with self.assertRaisesRegex(ValueError, "still exists after deletion"):
                    guest.drop_build_access("motorbuild", Path(directory), Path(directory) / "sudoers")

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
