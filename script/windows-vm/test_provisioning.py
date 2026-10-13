"""Tests for Windows VM provisioning on the Linux host.

They check what the seed CD contains for a standing VM and for a disposable
job VM (the job seed carries the agent source and no SSH keys). They also run
the standing VM's launcher against a real flock on the shared slot lock, so it
boots only while no job VM holds the lock and names the holder when one does.
"""
import importlib.util
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
TEST_PINS = ("SHA256:" + "A" * 43, "SHA256:" + "B" * 43)


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare = load("prepare_seed", "prepare-seed.py")
holder = load("slot_holder", "slot-holder.py")


class SeedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.pins = self.root / "trusted-fingerprints"
        self.pins.write_text("\n".join(TEST_PINS) + "\n")
        (self.root / "autounattend.xml").write_text(
            '<answer><!-- __BOOTSTRAP_PASSWORD__ -->' +
            '<password>__BOOTSTRAP_PASSWORD__</password>' * 3 + '</answer>')
        for name in ("bootstrap.ps1", "setup-guest.ps1", "setup-job-baseline.ps1", "dev-shell.ps1", "smoke.ps1"):
            shutil.copyfile(HERE / name, self.root / name)
        self.agent = self.root / "agent"
        shutil.copytree(HERE.parents[1] / "crates/motor-winagent", self.agent,
                        ignore=shutil.ignore_patterns("target"))
        # Build output and local configuration must never reach the setup CD.
        (self.agent / "target").mkdir(exist_ok=True)
        (self.agent / "target/secret").write_text("not source")
        (self.agent / ".cargo").mkdir()
        (self.agent / ".cargo/credentials.toml").write_text("not source")

    def render(self, *args):
        with patch.object(prepare, "ROOT", self.root), patch.object(sys, "argv", ["prepare-seed.py", *args]), \
                patch.object(prepare.subprocess, "run") as iso:
            prepare.main()
            self.assertEqual(iso.call_args.args[0][-1], str(self.root / "seed"))
            self.assertTrue(iso.call_args.kwargs["check"])

    def test_job_seed_contains_locked_agent_and_no_ssh_keys(self):
        with patch.object(Path, "home", side_effect=AssertionError("job mode read SSH keys")), \
                patch.object(prepare.subprocess, "check_output", side_effect=AssertionError("job mode ran ssh-keygen")):
            self.render("--job-baseline", "--agent-source", str(self.agent))
        seed = self.root / "seed"
        self.assertEqual({p.name for p in seed.iterdir()},
                         {"autounattend.xml", "job-baseline", "bootstrap.ps1", "setup-guest.ps1",
                          "setup-job-baseline.ps1", "motor-winagent"})
        for name in ("Cargo.toml", "Cargo.lock", "src/main.rs", "src/lib.rs"):
            self.assertEqual((seed / "motor-winagent" / name).read_bytes(), (self.agent / name).read_bytes())
        answer = (seed / "autounattend.xml").read_text()
        self.assertNotIn("__BOOTSTRAP_PASSWORD__", answer)
        self.assertEqual(answer.count("Aa1!"), 4)
        self.assertEqual((seed.stat().st_mode & 0o777), 0o700)
        self.assertFalse((seed / "motor-winagent/target").exists())
        self.assertFalse((seed / "motor-winagent/.cargo").exists())

    def test_job_seed_requires_standalone_lockfile_before_writing(self):
        (self.agent / "Cargo.lock").unlink()
        with self.assertRaisesRegex(SystemExit, "Missing motor-winagent Cargo.lock"):
            self.render("--job-baseline", "--agent-source", str(self.agent))
        self.assertFalse((self.root / "seed").exists())

    def test_standing_seed_preserves_two_key_validation_and_scripts(self):
        home = self.root / "home"
        (home / ".ssh").mkdir(parents=True)
        keys = "ssh-ed25519 first operator\nssh-ed25519 second motor\n"
        (home / ".ssh/authorized_keys").write_text(keys)
        fingerprints = "\n".join(f"256 {fp} owner (ED25519)" for fp in TEST_PINS)
        with patch.object(Path, "home", return_value=home), \
                patch.object(prepare.subprocess, "check_output", return_value=fingerprints):
            self.render("--key-fingerprints", str(self.pins))
        seed = self.root / "seed"
        self.assertEqual((seed / "authorized_keys").read_text(), keys)
        self.assertEqual({p.name for p in seed.iterdir()},
                         {"autounattend.xml", "authorized_keys", "bootstrap.ps1", "setup-guest.ps1", "dev-shell.ps1", "smoke.ps1"})

    def test_standing_seed_refuses_changed_fingerprints(self):
        home = self.root / "home"
        (home / ".ssh").mkdir(parents=True)
        (home / ".ssh/authorized_keys").write_text("ssh-ed25519 first\nssh-ed25519 second\n")
        with patch.object(Path, "home", return_value=home), \
                patch.object(prepare.subprocess, "check_output", return_value="256 wrong owner (ED25519)"), \
                self.assertRaisesRegex(SystemExit, "fingerprints changed"):
            self.render("--key-fingerprints", str(self.pins))
        self.assertFalse((self.root / "seed").exists())

    def test_standing_seed_requires_explicit_readable_pins(self):
        with self.assertRaisesRegex(SystemExit, "Supply --key-fingerprints"):
            self.render()
        with self.assertRaisesRegex(SystemExit, "Cannot read key fingerprint pins"):
            self.render("--key-fingerprints", str(self.root / "missing"))
        self.assertFalse((self.root / "seed").exists())

    def test_standing_seed_refuses_empty_duplicate_and_malformed_pins(self):
        for pins in ("", "# no pins\n", TEST_PINS[0] + "\n" + TEST_PINS[0], "wrong\n" + TEST_PINS[1]):
            with self.subTest(pins=pins):
                self.pins.write_text(pins)
                with self.assertRaisesRegex(SystemExit, "two distinct SHA256"):
                    self.render("--key-fingerprints", str(self.pins))
                self.assertFalse((self.root / "seed").exists())

    def test_existing_seed_is_never_overwritten(self):
        (self.root / "seed").mkdir()
        with self.assertRaisesRegex(SystemExit, "Setup CD already exists"):
            self.render("--job-baseline", "--agent-source", str(self.agent))


class HolderTests(unittest.TestCase):
    def test_holder_uses_kernel_inode_and_names_unit(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            lock = root / "slot.lock"
            lock.touch()
            info = lock.stat()
            proc = root / "proc"
            (proc / "123").mkdir(parents=True)
            (proc / "123/comm").write_text("qemu-system-x86\n")
            (proc / "123/cgroup").write_text("0::/system.slice/motor-windows-job-abc.service\n")
            (proc / "locks").write_text(
                '1: FLOCK ADVISORY WRITE 9 00:00:1 0 EOF\n' +
                f'2: FLOCK ADVISORY WRITE 123 {os.major(info.st_dev):02x}:{os.minor(info.st_dev):02x}:{info.st_ino} 0 EOF\n')
            self.assertEqual(holder.describe_holder(lock, proc),
                             "PID 123 (qemu-system-x86) unit motor-windows-job-abc.service")
            (proc / "123/comm").unlink()
            self.assertEqual(holder.describe_holder(lock, proc), "PID 123")


class BaselineInputsTests(unittest.TestCase):
    """Check required setup commands/paths, not whether installers work in a Windows VM."""

    def test_baseline_installs_pinned_machine_wide_read_only_rust(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        for expected in ("$toolchain = '1.99.0'", "$hostTriple = 'x86_64-pc-windows-msvc'",
                         "$env:RUSTUP_HOME = 'C:\\rust\\rustup'", "$env:CARGO_HOME = 'C:\\rust\\cargo'",
                         '--default-toolchain $toolchain --default-host $hostTriple --no-modify-path',
                         "SetEnvironmentVariable('RUSTUP_HOME', $env:RUSTUP_HOME, 'Machine')",
                         "'C:\\rust' /inheritance:r", ':(OI)(CI)RX'):
            self.assertIn(expected, source)

    def test_baseline_inputs_include_driver_identity_service_and_hardening(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        for expected in ('vioserial\\2k22\\amd64\\vioser.inf', '& pnputil /add-driver',
                         "$jobUser = 'motor-job'", 'New-LocalUser $jobUser',
                         'Remove-LocalGroupMember $admins', 'SeBatchLogonRight',
                         'CreateProfile($jobSid', 'job-password.txt', '$credential /inheritance:r',
                         'build --release --locked', 'New-Service -Name motor-winagent',
                         '-StartupType Automatic', "StartName -ne 'LocalSystem'", 'Get-FileHash',
                         'config --system core.longpaths true', 'LongPathsEnabled -PropertyType DWord -Value 1',
                         "@('C:\\m', 'C:\\rust', $jobCargo)", 'Add-MpPreference -ExclusionPath $path',
                         'Stop-Service sshd', 'Remove-WindowsCapability -Online -Name OpenSSH.Server',
                         "'C:\\ProgramData\\ssh'", 'authorized_keys', "Join-Path $_.FullName '.ssh'"):
            self.assertIn(expected, source)


@unittest.skipUnless(sys.platform == "linux" and shutil.which("flock"), "requires Linux flock and /proc/locks")
class LaunchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "provision").mkdir()
        shutil.copyfile(HERE / "slot-holder.py", self.root / "provision/slot-holder.py")
        self.slot = self.root / "slot.lock"
        self.slot.touch()
        self.qemu = self.root / "qemu"
        self.qemu.write_text('#!/usr/bin/python3\nimport os, sys\nprint("BOOTED " + str(os.getpid()), flush=True)\nsys.stdin.read()\n')
        self.qemu.chmod(0o700)
        self.launch = self.root / "launch.sh"
        self.launch.write_text((HERE / "launch.sh").read_text()
                               .replace("root=/srv/build/windows-vm", f"root={self.root}")
                               .replace("slot=/srv/build/motor-windows/slot.lock", f"slot={self.slot}")
                               .replace("/usr/bin/qemu-system-x86_64", str(self.qemu)))

    def start(self):
        vm = subprocess.Popen(["bash", str(self.launch)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop, vm)
        assert vm.stdout is not None
        with selectors.DefaultSelector() as poll:
            poll.register(vm.stdout, selectors.EVENT_READ)
            self.assertTrue(poll.select(timeout=5), "VM never booted")
        boot = vm.stdout.readline()
        self.assertTrue(boot.startswith("BOOTED "), boot)
        return vm, int(boot.split()[1])

    @staticmethod
    def stop(vm):
        if vm.poll() is None:
            if vm.stdin and not vm.stdin.closed:
                vm.stdin.close()
                vm.stdin = None
            try:
                vm.wait(timeout=5)
            except subprocess.TimeoutExpired:
                vm.kill()
                vm.wait(timeout=5)
        for stream in (vm.stdin, vm.stdout, vm.stderr):
            if stream and not stream.closed:
                stream.close()

    def test_standing_vm_holds_slot_for_entire_qemu_lifetime_and_names_holder(self):
        vm, pid = self.start()
        contender = subprocess.run(["bash", str(self.launch)], input="", capture_output=True, text=True, timeout=5)
        self.assertEqual(contender.returncode, 75)
        self.assertEqual(contender.stdout, "")
        self.assertIn(f"is held by PID {pid}", contender.stderr)
        self.assertIn("refusing to boot", contender.stderr)
        probe = subprocess.run(["flock", "-n", "-E", "75", str(self.slot), "true"])
        self.assertEqual(probe.returncode, 75)
        self.stop(vm)
        self.assertEqual(vm.returncode, 0)
        self.assertEqual(subprocess.run(["flock", "-n", str(self.slot), "true"]).returncode, 0)

    def test_standing_vm_refuses_boot_when_job_owns_slot(self):
        job = subprocess.Popen(["flock", "-n", "--no-fork", str(self.slot), str(self.qemu)],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop, job)
        assert job.stdout is not None
        with selectors.DefaultSelector() as poll:
            poll.register(job.stdout, selectors.EVENT_READ)
            self.assertTrue(poll.select(timeout=5))
        pid = int(job.stdout.readline().split()[1])
        result = subprocess.run(["bash", str(self.launch)], input="", capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 75)
        self.assertEqual(result.stdout, "")
        self.assertIn(f"is held by PID {pid}", result.stderr)

    def test_missing_slot_fails_closed_without_boot(self):
        self.slot.unlink()
        result = subprocess.run(["bash", str(self.launch)], capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("provision the shared slot lock", result.stderr)

    def test_qemu_failure_preserves_exit_status_and_releases_slot(self):
        self.qemu.write_text("#!/bin/sh\nexit 9\n")
        result = subprocess.run(["bash", str(self.launch)], capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 9)
        self.assertNotIn("held by", result.stderr)
        self.assertEqual(subprocess.run(["flock", "-n", str(self.slot), "true"]).returncode, 0)


if __name__ == "__main__":
    unittest.main()
