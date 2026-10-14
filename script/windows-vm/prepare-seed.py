#!/usr/bin/env python3
"""Render a setup CD for the persistent SSH VM or a disposable job image without SSH."""
from pathlib import Path
import argparse
import os
import re
import secrets
import shutil
import subprocess
import xml.etree.ElementTree as ET

from powershell_archive import ARCHIVE_NAME, verify_archive

ROOT = Path("/srv/build/windows-vm/provision")
NAMESPACE = "urn:schemas-microsoft-com:unattend"
WCM = "http://schemas.microsoft.com/WMIConfig/2002/State"


def job_answer(answer):
    """Register elevated bootstrap during Setup, not from the interactive account."""
    tag = lambda name: f"{{{NAMESPACE}}}{name}"
    tree = ET.fromstring(answer)
    shell = tree.find(f"{tag('settings')}[@pass='oobeSystem']/{tag('component')}[@name='Microsoft-Windows-Shell-Setup']")
    first_logon = shell.find(tag("FirstLogonCommands"))
    shell.remove(first_logon)
    specialize = tree.find(f"{tag('settings')}[@pass='specialize']")
    component = ET.SubElement(specialize, tag("component"), {
        "name": "Microsoft-Windows-Deployment", "processorArchitecture": "amd64",
        "publicKeyToken": "31bf3856ad364e35", "language": "neutral", "versionScope": "nonSxS",
    })
    synchronous = ET.SubElement(component, tag("RunSynchronous"))
    command = ET.SubElement(synchronous, tag("RunSynchronousCommand"), {f"{{{WCM}}}action": "add"})
    ET.SubElement(command, tag("Order")).text = "1"
    ET.SubElement(command, tag("Description")).text = "Register SYSTEM job-image bootstrap"
    ET.SubElement(command, tag("Path")).text = (
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe "
        r"-NoProfile -ExecutionPolicy Bypass -File D:\register-job-bootstrap.ps1"
    )
    ET.register_namespace("", NAMESPACE)
    ET.register_namespace("wcm", WCM)
    return ET.tostring(tree, encoding="unicode")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--job-baseline", action="store_true", help="Provision the SSH-free job image")
    parser.add_argument("--key-fingerprints", type=Path,
                        help="Required for the SSH VM: file containing the two trusted SHA256 fingerprints, one per line")
    parser.add_argument("--agent-source", type=Path,
                        default=Path(__file__).resolve().parents[2] / "crates/motor-winagent",
                        help="Standalone motor-winagent source directory (required on a script-only host)")
    args = parser.parse_args()
    os.umask(0o077)
    if (ROOT / "seed.iso").exists() or (ROOT / "seed").exists():
        raise SystemExit("Setup CD already exists; remove it deliberately before rendering again")
    lines = []
    if args.job_baseline:
        try:
            verify_archive(ROOT / ARCHIVE_NAME)
        except (OSError, ValueError) as error:
            raise SystemExit(f"Cannot stage pinned PowerShell ZIP: {error}") from error
        for name in ("Cargo.toml", "Cargo.lock", "src"):
            if not (args.agent_source / name).exists():
                raise SystemExit(f"Missing motor-winagent {name}; supply --agent-source with the standalone crate")
    else:
        if args.key_fingerprints is None:
            raise SystemExit("Supply --key-fingerprints with the trusted operator/motor key pins")
        try:
            pins = [line.strip() for line in args.key_fingerprints.read_text().splitlines()
                    if line.strip() and not line.lstrip().startswith("#")]
        except OSError as error:
            raise SystemExit(f"Cannot read key fingerprint pins: {error}") from error
        if len(pins) != 2 or len(set(pins)) != 2 or any(
                not re.fullmatch(r"SHA256:[A-Za-z0-9+/]{43}", pin) for pin in pins):
            raise SystemExit("Expected exactly two distinct SHA256 key fingerprint pins")
        keys = Path.home() / ".ssh/authorized_keys"
        lines = [line for line in keys.read_text().splitlines() if line.strip() and not line.startswith("#")]
        if len(lines) != 2 or any(not line.startswith("ssh-ed25519 ") for line in lines):
            raise SystemExit("Expected exactly the two plain ED25519 operator/motor public keys")
        result = subprocess.check_output(["ssh-keygen", "-lf", str(keys), "-E", "sha256"], text=True)
        if {line.split()[1] for line in result.splitlines()} != set(pins):
            raise SystemExit("Public key fingerprints changed; confirm their owners before provisioning")
    template = (ROOT / "autounattend.xml").read_text()
    if template.count("__BOOTSTRAP_PASSWORD__") != 4:
        # One occurrence is in the template's explanatory comment.
        raise SystemExit("Unexpected unattended template")
    password = "Aa1!" + secrets.token_urlsafe(36)
    answer = template.replace("__BOOTSTRAP_PASSWORD__", password)
    if args.job_baseline:
        answer = job_answer(answer)
    ET.fromstring(answer)
    seed = ROOT / "seed"
    seed.mkdir(mode=0o700)
    (seed / "autounattend.xml").write_text(answer)
    if args.job_baseline:
        # Tell bootstrap to omit SSH even when the unattended command has no mode flag.
        (seed / "job-baseline").touch()
        scripts = ("bootstrap.ps1", "setup-guest.ps1", "setup-job-baseline.ps1", "install-powershell.ps1",
                   "register-job-bootstrap.ps1", "finalize-job-baseline.ps1")
        shutil.copyfile(ROOT / ARCHIVE_NAME, seed / ARCHIVE_NAME)
        verify_archive(seed / ARCHIVE_NAME)
        agent = seed / "motor-winagent"
        agent.mkdir()
        for name in ("Cargo.toml", "Cargo.lock"):
            shutil.copyfile(args.agent_source / name, agent / name)
        shutil.copytree(args.agent_source / "src", agent / "src")
    else:
        (seed / "authorized_keys").write_text("\n".join(lines) + "\n")
        scripts = ("bootstrap.ps1", "setup-guest.ps1", "dev-shell.ps1", "smoke.ps1")
    for name in scripts:
        shutil.copyfile(ROOT / name, seed / name)
    subprocess.run(["genisoimage", "-quiet", "-J", "-r", "-V", "CKSETUP", "-o", str(ROOT / "seed.iso"), str(seed)], check=True)
    print("Setup CD rendered; delete seed/ and seed.iso after provisioning (temporary password)")


if __name__ == "__main__":
    main()
