"""Pinned system-service policy for headless job images, with measured outcomes."""

import hashlib
import json
from pathlib import Path
import plistlib
import re
import subprocess


POLICY = Path(__file__).with_name("guest-services-off.txt")
RESULTS = "# Provisioning results: "


def boot_session():
    value = subprocess.check_output(["/usr/sbin/sysctl", "-n", "kern.bootsessionuuid"], text=True).strip()
    if not re.fullmatch(r"[A-Fa-f0-9-]{36}", value):
        raise ValueError("cannot determine guest boot session")
    return value


def parse(text):
    services = {}
    for number, line in enumerate(text.splitlines(), 1):
        entry, _, reason = line.partition("#")
        if not entry.strip():
            continue
        fields = entry.split()
        if (len(fields) != 2 or fields[0] not in {"off", "kept-sip", "unverified"}
                or not re.fullmatch(r"com\.apple\.[A-Za-z0-9_.-]+", fields[1]) or not reason.strip()):
            raise ValueError(f"invalid service policy line {number}")
        expected, label = fields
        if label in services:
            raise ValueError("duplicate service label: " + label)
        services[label] = expected
    if not services:
        raise ValueError("empty service policy")
    return services


def installed_labels(directory):
    # A successful disable alone is not evidence of a real service: launchctl
    # accepts overrides for nonexistent labels too. Use Apple's system plists.
    labels = set()
    for path in directory.glob("*.plist"):
        with path.open("rb") as source:
            labels.add(plistlib.load(source).get("Label"))
    return labels


def apply(prefix, policy=POLICY, daemons=Path("/System/Library/LaunchDaemons")):
    text = policy.read_text()
    services = parse(text)
    labels = installed_labels(daemons)
    missing = services.keys() - labels
    if missing:
        raise ValueError("service labels absent from system LaunchDaemons: " + ", ".join(sorted(missing)))
    for label, expected in services.items():
        if expected == "kept-sip":
            continue
        subprocess.run(["/bin/launchctl", "disable", "system/" + label], check=True)
    # Do this before services are stopped by the next boot. mdutil needs mds to
    # answer, and launchctl disable persists across boots without booting out.
    subprocess.run(["/usr/bin/mdutil", "-a", "-i", "off"], check=True)
    receipt = {"policy_sha256": hashlib.sha256(text.encode()).hexdigest(),
               "provision_boot_session": boot_session(), "results": {}}
    write_results(prefix, text, receipt)
    return receipt


def write_results(prefix, text, receipt):
    (prefix / POLICY.name).write_text(text.rstrip() + "\n" + RESULTS + json.dumps(receipt, sort_keys=True) + "\n")


def running_services(output):
    section = re.search(r"^\s*services = \{\n(.*?)^\s*\}", output, re.M | re.S)
    if not section:
        raise ValueError("missing launchctl system services table")
    running = {}
    for row in section[1].splitlines():
        match = re.fullmatch(r"\s*(\d+)\s+(-|-?\d+)\s+(\S+)\s*", row)
        if not match:
            raise ValueError("invalid launchctl system services row: " + row)
        running[match[3]] = int(match[1])
    return running


def verify(prefix, policy=POLICY):
    text = policy.read_text()
    services = parse(text)
    installed = (prefix / POLICY.name).read_text()
    source, separator, result = installed.partition(RESULTS)
    if not separator or source.rstrip() != text.rstrip():
        raise ValueError("missing or changed guest service policy/results")
    receipt = json.loads(result)
    if receipt.get("policy_sha256") != hashlib.sha256(text.encode()).hexdigest():
        raise ValueError("guest service results differ from policy")
    current_boot = boot_session()
    if not receipt.get("provision_boot_session") or current_boot == receipt["provision_boot_session"]:
        raise ValueError("guest must reboot after applying the service policy")
    disabled = subprocess.check_output(["/bin/launchctl", "print-disabled", "system"], text=True)
    running = running_services(subprocess.check_output(["/bin/launchctl", "print", "system"], text=True))
    outcomes = {}
    for label, expected in services.items():
        is_disabled = bool(re.search(r'^\s*"' + re.escape(label) + r'"\s*=>\s*true\s*$', disabled, re.M))
        pid = running.get(label, 0)
        if expected == "kept-sip" or (expected == "unverified" and pid > 0):
            outcome = "kept-sip"
        elif is_disabled and pid == 0:
            outcome = "off"
        else:
            raise ValueError(f"service is not disabled or is still running: {label} (disabled={is_disabled}, pid={pid})")
        outcomes[label] = {"outcome": outcome, "disabled": is_disabled, "pid": pid}
    spotlight = subprocess.check_output(["/usr/bin/mdutil", "-s", "/"], text=True)
    if not re.fullmatch(r"/:\s*Indexing disabled\.\s*", spotlight):
        raise ValueError("Spotlight indexing is not disabled on /: " + spotlight)
    receipt.update(verified_boot_session=current_boot, results=outcomes)
    validate_receipt(receipt, text)
    write_results(prefix, text, receipt)
    return receipt


def validate_receipt(receipt, text=None):
    if text is None:
        text = POLICY.read_text()
    services = parse(text)
    if (receipt.get("policy_sha256") != hashlib.sha256(text.encode()).hexdigest()
            or not receipt.get("provision_boot_session") or not receipt.get("verified_boot_session")
            or receipt["provision_boot_session"] == receipt["verified_boot_session"]
            or set(receipt.get("results", {})) != set(services)):
        raise ValueError("guest service results differ from policy")
    for label, expected in services.items():
        actual = receipt["results"][label]
        if (set(actual) != {"outcome", "disabled", "pid"} or type(actual["disabled"]) is not bool
                or type(actual["pid"]) is not int or actual["pid"] < 0):
            raise ValueError("invalid service state: " + label)
        off = actual == {"outcome": "off", "disabled": True, "pid": 0} and expected != "kept-sip"
        kept = (actual["outcome"] == "kept-sip" and expected != "off"
                and (expected == "kept-sip" or actual["pid"] > 0))
        if not (off or kept):
            raise ValueError("unresolved or unexpected service outcome: " + label)
