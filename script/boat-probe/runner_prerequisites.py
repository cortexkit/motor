"""Measure Boat host prerequisites before selecting an image installation strategy."""

import json
import hashlib
import re
import shlex
import urllib.request
from credential_audit import SOURCE
from probe import provider_sizing_matches, sizing_error

GUEST = r'''
import fcntl, json, os, pathlib, subprocess

def command(argv):
    try:
        p = subprocess.run(argv, capture_output=True, text=True, timeout=30)
        return dict(argv=argv, exit_code=p.returncode, stdout=p.stdout, stderr=p.stderr)
    except (OSError, subprocess.TimeoutExpired) as e:
        return dict(argv=argv, error=str(e))

out = {"os_release": pathlib.Path("/etc/os-release").read_text(),
       "systemd": command(["systemctl", "--version"]),
       "private_pids": command(["systemd-run", "--wait", "--collect",
                                  "-p", "PrivatePIDs=yes", "/bin/true"]),
       "pid1_exe": os.readlink("/proc/1/exe"),
       "mounts": command(["findmnt", "-J", "/"]),
       "sshd_effective": command(["/usr/sbin/sshd", "-T"]),
       "sshd_unit": command(["systemctl", "cat", "ssh"]),
       "sshd_processes": command(["ps", "-C", "sshd", "-o", "pid,ppid,user,comm"]),
       "loop": command(["losetup", "--find"])}
out["systemd_container"] = {
    "package": command(["apt-cache", "policy", "systemd-container"]),
    "installed": command(["dpkg-query", "-W", "systemd-container"]),
    "nspawn": command(["systemd-nspawn", "--version"])}
cgroup_script = """import json, pathlib
p = pathlib.Path("/sys/fs/cgroup") / pathlib.Path("/proc/self/cgroup").read_text().strip().split("0::", 1)[1].lstrip("/")
print(json.dumps({"path": str(p), **{n: (p/n).read_text().strip() if (p/n).exists() else None for n in ("cgroup.controllers", "cgroup.subtree_control", "memory.max", "cpu.max", "pids.max")}}))"""
out["system_unit_cgroup"] = command(["systemd-run", "--wait", "--pipe", "--collect",
    "-p", "Delegate=cpu memory pids", "python3", "-c", cgroup_script])
out["ssh_config"] = {}
for path in [pathlib.Path("/etc/ssh/sshd_config"), *pathlib.Path("/etc/ssh/sshd_config.d").glob("*.conf")]:
    out["ssh_config"][str(path)] = path.read_text()
try:
    fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
    try:
        out["kvm"] = dict(opened=True, api_version=fcntl.ioctl(fd, 0xAE00, 0))
    finally:
        os.close(fd)
except OSError as e:
    out["kvm"] = dict(opened=False, errno=e.errno, error=str(e))
out["nested"] = {}
for name in ("kvm_intel", "kvm_amd"):
    path = pathlib.Path("/sys/module") / name / "parameters/nested"
    if path.exists():
        out["nested"][name] = path.read_text().strip()
out["cpu_virtualization_flags"] = sorted(set(
    flag for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines()
    if line.startswith("flags") for flag in line.split(":", 1)[1].split() if flag in {"vmx", "svm"}))
print(json.dumps(out))
'''
GUEST = SOURCE + GUEST.replace('print(json.dumps(out))', 'out["credential_audit"] = credential_audit()\nprint(json.dumps(out))')


def create_schema(text):
    match = re.search(r"(?ms)^    CreateSandboxRequest:\n(.*?)(?=^    [A-Za-z][^\n]*:\n)", text)
    if not match:
        raise ValueError("cannot locate CreateSandboxRequest in public Boat schema")
    fields = re.findall(r"(?m)^        ([A-Za-z][A-Za-z0-9]*):\s*$", match[1])
    if not {"from", "type", "noEnv", "environment"} <= set(fields):
        raise ValueError("unrecognized Boat create schema")
    return {"url": "https://docs.boat.dev/openapi/boat-v1.yaml", "sha256": hashlib.sha256(text.encode()).hexdigest(),
            "fields": fields, "schema_excerpt": match[0],
            "documented_base_selectors": [f for f in fields if f in {"image", "os", "template", "baseImage"}]}


def parse(status):
    if status.get("exitCode") != 0 or status.get("stdoutTruncated") or status.get("stderrTruncated"):
        raise ValueError("prerequisite command failed or was truncated: " + json.dumps(status))
    data = json.loads(status["stdout"])
    if not isinstance(data.get("os_release"), str) or not isinstance(data.get("private_pids"), dict):
        raise ValueError("missing prerequisite evidence")
    return data


def run(probe):
    with urllib.request.urlopen("https://docs.boat.dev/openapi/boat-v1.yaml", timeout=30) as response:
        probe.data["boat_create_schema"] = create_schema(response.read().decode())
    probe.checkpoint()
    item, probe.data["create"] = probe.create()
    if not provider_sizing_matches(item, probe.size):
        raise ValueError(sizing_error(probe.size))
    probe.data["runner_prerequisites"] = parse(probe.execute(
        item["id"], "sudo -n -E python3 -c " + shlex.quote(GUEST), poll_seconds=120))
    probe.checkpoint()
