#!/usr/bin/env python3
"""Stage the existing SSH-free job seed with explicit Hyper-V deployment changes."""
import argparse
import hashlib
import importlib.util
from pathlib import Path
import secrets
import shutil
import sys
import xml.etree.ElementTree as ET

REPO = Path(__file__).resolve().parents[2]
SETUP = REPO / "script/windows-vm"
def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"Cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


archive_module = load("hyperv_powershell_archive", SETUP / "powershell_archive.py")
ARCHIVE_NAME = archive_module.ARCHIVE_NAME
verify_archive = archive_module.verify_archive
sys.path.insert(0, str(SETUP))
try:
    original = load("motor_windows_seed", SETUP / "prepare-seed.py")
finally:
    sys.path.pop(0)
NS = original.NAMESPACE
TAG = lambda name: f"{{{NS}}}{name}"


def answer(template, password):
    if template.count("__BOOTSTRAP_PASSWORD__") != 4:
        raise ValueError("Unexpected unattended template")
    root = ET.fromstring(original.job_answer(template.replace("__BOOTSTRAP_PASSWORD__", password)))
    # DISM applies image index 2; host Storage cmdlets create the UEFI GPT disk.
    # Windows PE never runs in this deployment, so omit its BIOS/virtio settings.
    windows_pe = root.find(f"{TAG('settings')}[@pass='windowsPE']")
    if windows_pe is None:
        raise ValueError("Windows PE settings missing")
    root.remove(windows_pe)
    command = root.find(f"{TAG('settings')}[@pass='specialize']/{TAG('component')}[@name='Microsoft-Windows-Deployment']/{TAG('RunSynchronous')}/{TAG('RunSynchronousCommand')}/{TAG('Path')}")
    if command is None:
        raise ValueError("Job bootstrap registration command missing")
    command.text = (
        'cmd.exe /c for %d in (D E F G H I J K L M N O P Q R S T U V W X Y Z) do '
        '@if exist %d:\\register-job-bootstrap.ps1 powershell.exe -NoProfile '
        '-ExecutionPolicy Bypass -File %d:\\register-job-bootstrap.ps1'
    )
    return ET.tostring(root, encoding="unicode")


def stage(destination, archive, guest_agent=None, peer_runner=None):
    if (guest_agent is None) != (peer_runner is None):
        raise ValueError("Prebuilt hvsock agent and peer runner must be supplied together")
    if guest_agent is not None and (not guest_agent.is_file() or not peer_runner.is_file()):
        raise ValueError("Both prebuilt executables must exist")
    if destination.exists():
        raise ValueError("Seed already exists; remove deliberately after saving logs")
    verify_archive(archive)
    destination.mkdir(parents=True)
    (destination / "autounattend.xml").write_text(answer(
        # No "-": the template also names the placeholder in an XML comment, where
        # "--" is illegal, so a password with two in a row broke the answer file.
        (SETUP / "autounattend.xml").read_text(), "Aa1!" + secrets.token_urlsafe(36).replace("-", "_")),
        encoding="utf-8")
    for marker in ("job-baseline", "hyperv-spike"):
        (destination / marker).touch()
    scripts = ("bootstrap.ps1", "setup-guest.ps1", "setup-job-baseline.ps1", "install-powershell.ps1",
               "register-job-bootstrap.ps1", "finalize-job-baseline.ps1")
    for name in scripts:
        shutil.copyfile(SETUP / name, destination / name)
        digest = hashlib.sha256((destination / name).read_bytes()).hexdigest()
        print(f"PASS seed-copy {name} SHA256={digest}")
    shutil.copyfile(archive, destination / ARCHIVE_NAME)
    verify_archive(destination / ARCHIVE_NAME)
    if guest_agent is not None:
        (destination / "hyperv-runner").touch()
        shutil.copyfile(guest_agent, destination / "motor-winagent-prebuilt.exe")
        shutil.copyfile(peer_runner, destination / "motor-runner-prebuilt.exe")
        print("PASS seed-stage prebuilt hvsock runner image; no agent build in guest")
        return
    agent = REPO / "crates/motor-winagent"
    target = destination / "motor-winagent"
    target.mkdir()
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copyfile(agent / name, target / name)
    for name in ("src", "examples"):
        shutil.copytree(agent / name, target / name)
    print("PASS seed-stage (contains one-time password; do not publish)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", required=True, type=Path)
    parser.add_argument("--powershell-archive", required=True, type=Path)
    parser.add_argument("--guest-agent", type=Path)
    parser.add_argument("--peer-runner", type=Path)
    args = parser.parse_args()
    try:
        stage(args.destination, args.powershell_archive, args.guest_agent, args.peer_runner)
    except (OSError, ValueError) as error:
        raise SystemExit(f"FAIL seed-stage {error}") from error


if __name__ == "__main__":
    main()
