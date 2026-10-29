"""Tests for Windows VM provisioning on the Linux host.

They check what the seed CD contains for a standing VM and for a disposable
job VM (the job seed carries the agent source and no SSH keys). They also run
the standing VM's launcher against a real flock on the shared slot lock, so it
boots only while no job VM holds the lock and names the holder when one does.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
TEST_PINS = ("SHA256:" + "A" * 43, "SHA256:" + "B" * 43)


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


media = load("powershell_archive", "powershell_archive.py")
prepare = load("prepare_seed", "prepare-seed.py")
holder = load("slot_holder", "slot-holder.py")


class SeedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.pins = self.root / "trusted-fingerprints"
        self.pins.write_text("\n".join(TEST_PINS) + "\n")
        shutil.copyfile(HERE / "autounattend.xml", self.root / "autounattend.xml")
        for name in ("bootstrap.ps1", "setup-guest.ps1", "setup-job-baseline.ps1", "install-powershell.ps1",
                     "register-job-bootstrap.ps1", "finalize-job-baseline.ps1", "dev-shell.ps1", "smoke.ps1"):
            shutil.copyfile(HERE / name, self.root / name)
        (self.root / media.ARCHIVE_NAME).write_bytes(b"test PowerShell media")
        self.hash_patch = patch.object(media, "ARCHIVE_SHA256", hashlib.sha256(b"test PowerShell media").hexdigest())
        self.hash_patch.start()
        self.addCleanup(self.hash_patch.stop)
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
                           "setup-job-baseline.ps1", "install-powershell.ps1", "register-job-bootstrap.ps1",
                           "finalize-job-baseline.ps1", media.ARCHIVE_NAME, "motor-winagent"})
        self.assertEqual((seed / media.ARCHIVE_NAME).read_bytes(), b"test PowerShell media")
        for name in ("Cargo.toml", "Cargo.lock", "src/main.rs", "src/lib.rs"):
            self.assertEqual((seed / "motor-winagent" / name).read_bytes(), (self.agent / name).read_bytes())
        answer = (seed / "autounattend.xml").read_text()
        self.assertNotIn("__BOOTSTRAP_PASSWORD__", answer)
        self.assertEqual(answer.count("Aa1!"), 3)
        tree = ET.fromstring(answer)
        ns = {"u": prepare.NAMESPACE}
        self.assertIsNone(tree.find(".//u:FirstLogonCommands", ns))
        path = tree.find("u:settings[@pass='specialize']/u:component[@name='Microsoft-Windows-Deployment']/u:RunSynchronous/u:RunSynchronousCommand/u:Path", ns)
        self.assertIn(r"-File D:\register-job-bootstrap.ps1", path.text)
        self.assertEqual((seed.stat().st_mode & 0o777), 0o700)
        self.assertFalse((seed / "motor-winagent/target").exists())
        self.assertFalse((seed / "motor-winagent/.cargo").exists())

    def test_password_with_hyphens_still_renders_a_well_formed_answer(self):
        # token_urlsafe can emit "--", which XML forbids inside the template's
        # comment that names the placeholder.
        with patch.object(prepare.secrets, "token_urlsafe", return_value="ab--cd-" * 7), \
                patch.object(Path, "home", side_effect=AssertionError("job mode read SSH keys")):
            self.render("--job-baseline", "--agent-source", str(self.agent))
        answer = (self.root / "seed/autounattend.xml").read_text()
        ET.fromstring(answer)
        self.assertNotIn("-", prepare.bootstrap_password())

    def test_job_seed_rejects_missing_or_wrong_powershell_before_writing(self):
        archive = self.root / media.ARCHIVE_NAME
        for missing in (False, True):
            if missing:
                archive.unlink()
            else:
                archive.write_bytes(b"wrong media")
            with self.assertRaisesRegex(SystemExit, "Cannot stage pinned PowerShell ZIP"):
                self.render("--job-baseline", "--agent-source", str(self.agent))
            self.assertFalse((self.root / "seed").exists())

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
        self.assertIn("FirstLogonCommands", (seed / "autounattend.xml").read_text())
        self.assertNotIn("register-job-bootstrap.ps1", (seed / "autounattend.xml").read_text())
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
    def test_job_symlink_right_is_sid_scoped_and_verified_without_developer_mode(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        self.assertIn("@('SeBatchLogonRight', 'SeCreateSymbolicLinkPrivilege')", source)
        self.assertIn('if ($grantSids -notcontains $jobSid) { $grants += "*$jobSid" }', source)
        self.assertIn('if ($grantSids -notcontains $jobSid) { throw "Job SID missing from $right" }', source)
        self.assertIn("if ($developerMode -eq 1) { throw 'Job baseline must not enable Developer Mode' }", source)
        self.assertEqual(source.count('& secedit /export /cfg $policy /areas USER_RIGHTS /quiet'), 2)

    """Check required setup commands/paths, not whether installers work in a Windows VM."""

    def test_baseline_installs_pinned_machine_wide_read_only_rust(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        for expected in ("$toolchain = '1.99.0'", "$hostTriple = 'x86_64-pc-windows-msvc'",
                         "$env:RUSTUP_HOME = 'C:\\rust\\rustup'", "$env:CARGO_HOME = 'C:\\rust\\cargo'",
                         '--default-toolchain $toolchain --default-host $hostTriple --no-modify-path',
                         "SetEnvironmentVariable('RUSTUP_HOME', $env:RUSTUP_HOME, 'Machine')",
                         "'C:\\rust' /inheritance:r", ':(OI)(CI)RX'):
            self.assertIn(expected, source)

    def test_baseline_installs_checksum_pinned_bun_before_use(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        version = json.loads((HERE.parent / "runner/toolchain.json").read_text())["tools"]["bun"]["version"]
        self.assertEqual(version, "1.4.2")
        for expected in (f"$bunVersion = '{version}'",
                         "https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/SHASUMS256.txt",
                         "$bunSha256 = 'ce4c17497b2f29712a99d3d53f028de28cd42e3bacb8589599e7f000e49b6405'",
                         "Invoke-WebRequest 'https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-windows-x64.zip' -OutFile $bunArchive",
                         "if ((Get-FileHash -LiteralPath $bunArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $bunSha256) {\n        throw 'Bun archive SHA-256 mismatch'\n    }"):
            self.assertIn(expected, source)
        self.assertLess(source.index("Invoke-WebRequest 'https://github.com/oven-sh/bun/"), source.index("Get-FileHash -LiteralPath $bunArchive"))
        self.assertLess(source.index("throw 'Bun archive SHA-256 mismatch'"), source.index("Expand-Archive -LiteralPath $bunArchive"))
        self.assertLess(source.index("Expand-Archive -LiteralPath $bunArchive"), source.index('& "$bunInstall\\bun.exe" --version'))

    def test_baseline_bun_is_protected_and_on_machine_path(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        for expected in ("$bunInstall = 'C:\\Program Files\\Bun'",
                         "& icacls $bunInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'",
                         '& icacls "$bunInstall\\*" /reset /T /C /L /Q',
                         "& icacls $bunInstall /setowner '*S-1-5-32-544' /T /C /L /Q",
                         "[Environment]::SetEnvironmentVariable('Path', \"$bunInstall;$machinePath\", 'Machine')",
                         '$env:Path = "$bunInstall;" + $env:Path',
                         'if ($LASTEXITCODE -ne 0 -or $installedBunVersion -cne $bunVersion) { throw'):
            self.assertIn(expected, source)
        self.assertLess(source.index("& icacls $bunInstall /inheritance:r"), source.index("Expand-Archive -LiteralPath $bunArchive"))
        self.assertLess(source.index("& icacls $bunInstall /setowner"), source.index('& "$bunInstall\\bun.exe" --version'))

    def test_job_baseline_owns_bun_for_both_guest_install_paths(self):
        guest = (HERE / "setup-guest.ps1").read_text()
        self.assertIn("if ($JobBaseline) {\n    Write-Host 'Job baseline: machine-wide pinned Bun is installed by setup-job-baseline.ps1.'\n} elseif ($null -eq (Get-Command bun", guest)
        self.assertIn("$verifyTools = if ($JobBaseline) { @('git') }", guest)
        bootstrap = (HERE / "bootstrap.ps1").read_text()
        self.assertIn('setup-guest.ps1 -JobBaseline:$JobBaseline', bootstrap)
        self.assertIn('setup-job-baseline.ps1 -HyperVSpike:$HyperVSpike', bootstrap)
        baseline = (HERE / "setup-job-baseline.ps1").read_text().split("$bunVersion = ", 1)[1].split("$bytes = ", 1)[0]
        self.assertNotIn("$HasWinget", baseline)
        self.assertNotIn("$HyperVSpike", baseline)

    def test_windows_image_qualification_requires_bun_version(self):
        root = HERE.parent.parent
        operator = (root / "crates/motor-runner/src/tests/windows_operator.rs").read_text()
        self.assertIn('git --version; bun --version; pwsh', operator)
        self.assertIn('output.lines().any(|line| line.trim() == "1.4.2")', operator)
        provision = (HERE.parent / "hyperv-spike/provision.ps1").read_text()
        self.assertIn("Assert-Spike ($bootstrap -match 'Verified machine-wide Bun 1\\.4\\.2 with protected user read/execute access')", provision)

    def test_baseline_installs_and_verifies_rustfmt_and_clippy_for_all_images(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        self.assertIn("\n& $rustup component add clippy rustfmt --toolchain $toolchain\n", source)
        for expected in ("component list --installed --toolchain $toolchain",
                         "@('clippy', 'rustfmt')",
                         'if ($components -notcontains "$component-$hostTriple") { throw'):
            self.assertIn(expected, source)
        rust_setup = source.split("$rustup = ", 1)[1].split("$git = ", 1)[0]
        self.assertNotIn("$HyperVSpike", rust_setup)
        self.assertNotIn("hyperv-runner", rust_setup)

    def test_baseline_installs_and_verifies_both_windows_rust_targets(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        self.assertIn("\n& $rustup target add aarch64-pc-windows-msvc --toolchain $toolchain\n", source)
        for expected in ("target list --installed --toolchain $toolchain",
                         "@($hostTriple, 'aarch64-pc-windows-msvc')",
                         'if ($targets -notcontains $target) { throw'):
            self.assertIn(expected, source)

    def test_baseline_installs_checksum_pinned_nextest_beside_cargo_for_all_images(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        catalogue = json.loads((HERE.parent / "runner/toolchain.json").read_text())
        version = next(tool["version"] for tool in catalogue["cargo_installs"] if tool["crate"] == "cargo-nextest")
        self.assertEqual(version, "0.9.138")
        for expected in (f"$nextestVersion = '{version}'",
                         "$nextestSha256 = 'e4606198dd067299f16863dcf5b9be007b9cae041e468c417884fc224760c0f4'",
                         f"Invoke-WebRequest 'https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{version}/cargo-nextest-{version}-x86_64-pc-windows-msvc.tar.gz' -OutFile $nextestArchive",
                         "if ((Get-FileHash -LiteralPath $nextestArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $nextestSha256) {\n        throw 'cargo-nextest archive SHA-256 mismatch'\n    }",
                         r"""& "$env:SystemRoot\System32\tar.exe" -xzf $nextestArchive -C 'C:\rust\cargo\bin' cargo-nextest.exe""",
                         "if ($LASTEXITCODE -ne 0) { throw 'cargo-nextest extraction failed' }",
                         "Remove-Item -LiteralPath $nextestArchive -Force -ErrorAction SilentlyContinue",
                         r"$nextestOutput = @(& 'C:\rust\cargo\bin\cargo.exe' nextest --version)",
                         "if ($LASTEXITCODE -ne 0 -or [string]($nextestOutput | Select-Object -First 1) -notmatch ('^cargo-nextest ' + [regex]::Escape($nextestVersion) + '(?:\\s|$)')) {",
                         'throw "Machine-wide cargo-nextest version verification failed: '):
            self.assertIn(expected, source)
        self.assertLess(source.index("-OutFile $nextestArchive"), source.index("Get-FileHash -LiteralPath $nextestArchive"))
        self.assertLess(source.index("throw 'cargo-nextest archive SHA-256 mismatch'"), source.index("-xzf $nextestArchive"))
        self.assertLess(source.index("-xzf $nextestArchive"), source.index("nextest --version"))
        self.assertLess(source.index("nextest --version"), source.index("# Freeze toolchain files"))
        nextest_setup = source.split("$nextestVersion = ", 1)[1].split("$vswhere = ", 1)[0]
        self.assertNotIn("$HyperVSpike", nextest_setup)
        self.assertNotIn("hyperv-runner", nextest_setup)

    def test_windows_image_qualification_requires_nextest_version(self):
        operator = (HERE.parent.parent / "crates/motor-runner/src/tests/windows_operator.rs").read_text()
        self.assertIn("cargo --version; cargo nextest --version; git --version", operator)
        self.assertIn('line.starts_with("cargo-nextest 0.9.138 ")', operator)

    def test_baseline_installs_checksum_pinned_machine_wide_python_on_path(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        for expected in (
            "$pythonVersion = '3.13.17'",
            "https://www.python.org/ftp/python/3.13.17/python-3.13.17-amd64.exe.spdx.json",
            "$pythonSha256 = '5da012101ba2e17113a9a1cd901e9ce1441086f2b7ecb5de06ec94163899a190'",
            "Invoke-WebRequest 'https://www.python.org/ftp/python/3.13.17/python-3.13.17-amd64.exe' -OutFile $pythonInstaller",
            "Get-FileHash -LiteralPath $pythonInstaller -Algorithm SHA256",
            "throw 'Python installer SHA-256 mismatch'",
            "$pythonInstall = 'C:\\Python313'",
            "'/quiet', 'InstallAllUsers=1', 'PrependPath=0', 'Include_launcher=0', 'Include_test=0'",
            '"TargetDir=$pythonInstall"',
            "Start-Process -FilePath $pythonInstaller -ArgumentList $pythonArguments -Wait -PassThru",
            "Remove-Item -LiteralPath $pythonInstaller -Force -ErrorAction SilentlyContinue",
            r'Copy-Item -LiteralPath "$pythonInstall\python.exe" -Destination "$pythonInstall\python3.exe"',
            "& icacls $pythonInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'",
            r'& icacls "$pythonInstall\*" /reset /T /C /L /Q',
            "& icacls $pythonInstall /setowner '*S-1-5-32-544' /T /C /L /Q",
            "[Environment]::SetEnvironmentVariable('Path', \"$pythonInstall;$machinePath\", 'Machine')",
            '$env:Path = "$pythonInstall;" + $env:Path',
            r'& "$pythonInstall\$command" --version',
            r'& "$pythonInstall\python3.exe" -c "import json, hashlib, subprocess, tomllib"',
            "throw 'Machine-wide Python stdlib verification failed'",
        ):
            self.assertIn(expected, source)
        self.assertLess(source.index('-OutFile $pythonInstaller'), source.index('Get-FileHash -LiteralPath $pythonInstaller'))
        self.assertLess(source.index("throw 'Python installer SHA-256 mismatch'"), source.index('Start-Process -FilePath $pythonInstaller'))
        self.assertLess(source.index('& icacls $pythonInstall /inheritance:r'), source.index('Start-Process -FilePath $pythonInstaller'))
        self.assertLess(source.index('& icacls $pythonInstall /setowner'), source.index(r'& "$pythonInstall\$command" --version'))
        setup = source.split("$pythonVersion = ", 1)[1].split("$bytes = ", 1)[0]
        self.assertNotIn('$HyperVSpike', setup)
        self.assertNotIn('$HasWinget', setup)

    def test_windows_image_qualification_requires_python_version_and_stdlib(self):
        operator = (HERE.parent.parent / "crates/motor-runner/src/tests/windows_operator.rs").read_text()
        self.assertIn('python3 --version; python --version; python3 -c \\"import json, hashlib, subprocess, tomllib\\"', operator)
        self.assertIn('line.trim() == "Python 3.13.17"', operator)
        self.assertIn("@{name='Python';command='python3.exe';path='C:\\Python313'}", operator)
        self.assertIn('output.contains("Python installation is read-only")', operator)
        provision = (HERE.parent / "hyperv-spike/provision.ps1").read_text()
        self.assertIn("Assert-Spike ($bootstrap -match 'Verified machine-wide Python 3\\.13\\.17 with protected user read/execute access')", provision)
        self.assertIn("Assert-Spike ($bootstrap -match 'Verified machine-wide Python stdlib imports: json, hashlib, subprocess, tomllib')", provision)

    def test_guest_requests_arm64_msvc_on_every_install_path(self):
        source = (HERE / "setup-guest.ps1").read_text()
        winget = source.split("$vsArgs = @(", 1)[1].split(") -join", 1)[0]
        self.assertIn('"Microsoft.VisualStudio.Component.VC.Tools.ARM64"', winget)
        installs = re.findall(r'^\s*\$p = Start-Process .*vs_buildtools\.exe.*$', source, re.MULTILINE)
        self.assertEqual(len(installs), 2, "expected fresh installation and existing-instance repair")
        for install in installs:
            self.assertIn("--add Microsoft.VisualStudio.Component.VC.Tools.ARM64", install)
        self.assertIn('modify --quiet --wait', installs[0])
        self.assertIn('-requires Microsoft.VisualStudio.Workload.VCTools Microsoft.VisualStudio.Component.VC.Tools.ARM64', source)

    def test_baseline_checks_arm64_linker_and_builds_offline_executable(self):
        source = (HERE / "setup-job-baseline.ps1").read_text()
        for expected in ('-requires Microsoft.VisualStudio.Workload.VCTools Microsoft.VisualStudio.Component.VC.Tools.ARM64',
                         r'VC\Tools\MSVC\*\bin\Hostx64\arm64\link.exe',
                         "if (-not $arm64Linker) { throw",
                         'fn main() { println!("ARM64 toolchain verified"); }',
                         '[workspace]',
                         '"+$toolchain" build --offline --target aarch64-pc-windows-msvc',
                         'if ($LASTEXITCODE -ne 0) { throw "Offline ARM64 Cargo build failed:',
                         r'target\aarch64-pc-windows-msvc\debug\motor-arm64-toolchain-probe.exe'):
            self.assertIn(expected, source)
        manifest = source.split('Set-Content "$arm64Probe\\Cargo.toml"', 1)[1].split("'@", 1)[0]
        self.assertNotIn("[dependencies]", manifest)
        self.assertLess(source.index('build --offline'), source.index('# Freeze toolchain files'))

    def test_baseline_installs_pinned_protected_powershell(self):
        source = (HERE / "install-powershell.ps1").read_text()
        for expected in ("7.6.7", "d2f952ebdbf24386cb8923553aa58e41a846db35a6155ea04c58142a4b16dbb7",
                         "C:\\Program Files\\PowerShell\\7", "Get-FileHash", "Expand-Archive",
                         "/inheritance:r", "*S-1-5-32-545:(OI)(CI)RX", "/reset /T /C /L /Q",
                         "GetAccessRules", "PowerShell ACL inheritance reset failed",
                         "'$PSVersionTable.PSVersion.ToString()'", "Assert-MotorPowerShellProbe $probe '7.6.7'",
                         "RedirectStandardError = $true", "-NonInteractive -EncodedCommand"):
            self.assertIn(expected, source)
        self.assertIn('& "$PSScriptRoot\\install-powershell.ps1"', (HERE / "setup-job-baseline.ps1").read_text())
        self.assertIn('PowerShell-7.6.7-win-x64.zip', (HERE / "bootstrap.ps1").read_text())

    def test_powershell_machine_and_process_path_commands_are_present(self):
        source = (HERE / "install-powershell.ps1").read_text()
        self.assertIn("[Environment]::GetEnvironmentVariable('Path', 'Machine')", source)
        self.assertIn("-notcontains $destination", source)
        self.assertIn("[Environment]::SetEnvironmentVariable('Path', \"$destination;$machinePath\", 'Machine')", source)
        self.assertIn('$env:Path = "$destination;" + $env:Path', source)

    def test_job_bootstrap_schedules_system_finalization_and_records_license(self):
        register = (HERE / "register-job-bootstrap.ps1").read_text()
        bootstrap = (HERE / "bootstrap.ps1").read_text()
        finalize = (HERE / "finalize-job-baseline.ps1").read_text()
        for expected in ("New-ScheduledTaskTrigger -AtLogOn", "-User SYSTEM -RunLevel Highest", "MotorBootstrap"):
            self.assertIn(expected, register)
        for expected in ("Unregister-ScheduledTask -TaskName MotorBootstrap", "'/dlv', '/xpr'",
                         "New-ScheduledTaskTrigger -AtStartup", "-User SYSTEM -RunLevel Highest",
                         "Start-ScheduledTask -TaskName MotorFinalize"):
            self.assertIn(expected, bootstrap)
        for expected in ("ResultCode -ne 2", "HResult -ne 0", "Microsoft.Update.SystemInfo",
                         "Updates did not settle within five", "Set-Content $complete",
                         "Set-Content C:\\build\\provision\\FAILED.txt"):
            self.assertIn(expected, finalize)

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
