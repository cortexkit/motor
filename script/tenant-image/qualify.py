#!/usr/bin/env python3
"""Operator-only tenant image qualification through provisioning, helper and TLS admission."""
import argparse
import base64
import fcntl
import hashlib
import hmac
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import pwd
import re
import shutil
import signal
import socket
import ssl
import stat
import struct
import subprocess
import sys
import time
import traceback
import urllib.request
import uuid
from typing import Any

from build_storage import mounts_under, require_private_view

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
ROOT = Path("/var/lib/motor-vm")
REGISTRY = Path("/etc/motor-vm-helper/tenants")
BINDINGS = Path("/etc/motor-host/tenants.json")
HELPER_SOCKET = Path("/run/motor-vm-helper.sock")
MOUNTINFO = Path("/proc/self/mountinfo")
FORWARDING = Path("/proc/sys/net/ipv4/ip_forward")
FLOOR = 170 * 1024**3
GIB = 1024**3
LIVE = ("hosted image live inventory: 10 checks passed (overlay, read-only lower, tmpfs, size, "
        "PID 1, accounts, services, network, private writes, denied lower writes)")
TENANT = "hosted image tenant inventory: 3 checks passed (XFS, FICLONE to /Users and /home)"
NAME_SERVICE = ("PASS guest name service: no VM nameserver, host name from files, "
                "sudo and lookup fast with an unreachable resolver, runner ready within 10 s")
TOOL_TREES = "PASS guest tool trees: root-owned and world-readable without ACLs; no copy-up at runner start"
ROOT_PATHS = "PASS guest root paths: everything root executes from is root-owned without group or other write"
PARITY = "PASS guest toolchain parity: tenant host contract and package-qualified gnutest"
JAVASCRIPT_SETUP = ("PASS guest runner JavaScript setup: bun.lock, package-lock.json and pnpm-lock.yaml "
                    "installed with the tenant runner's tools after attach")
# Committed offline fixtures, one per lockfile kind the runner installs from.
# Every qualification job's repository carries them, so each job's real
# workspace setup runs Bun, npm and pnpm inside the booted image.
JAVASCRIPT_FIXTURES = REPO / "crates/motor-runner/test-fixtures/javascript"
JAVASCRIPT_MANAGERS = ("bun", "npm", "pnpm")
TEST = "isolation::tests::hosted_job_cannot_open_vsock"
WORKSPACE = "/Users/qualification/probe"
STEPS = ("preflight", "provision and boot", "live inventory", "tenant inventory", "name service",
          "tool trees", "root paths", "toolchain parity", "javascript setup", "vsock isolation", "first job", "warm target reuse", "suspend", "restore", "post-restore job",
         "guest units", "teardown")


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def b64(data):
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def describe_error(error):
    return f"{type(error).__name__}: {str(error) or repr(error)}"


def error_evidence(error):
    details: dict[str, Any] = dict(exception_type=type(error).__name__, exception_repr=repr(error), traceback=traceback.format_exc())
    if isinstance(error, (subprocess.CalledProcessError, subprocess.TimeoutExpired)):
        details["command"] = [str(arg) for arg in error.cmd] if isinstance(error.cmd, (list, tuple)) else str(error.cmd)
        details["returncode"] = getattr(error, "returncode", None)
        for name in ("stdout", "stderr"):
            value = getattr(error, name, None)
            details[name] = value.decode(errors="replace") if isinstance(value, bytes) else value
    return details


def unit_not_loaded(result):
    output = result.stdout or b""
    error = result.stderr or b""
    text = (output + error).decode(errors="replace").lower()
    return result.returncode in (1, 4, 5) and ("not loaded" in text or "could not be found" in text or "loadstate=not-found" in text)


def owned_unit(name, control_group, units, tenant):
    return name in units or tenant in PurePosixPath(control_group).parts


def remove_owned_tree(path):
    def missing_only(_function, _path, error):
        if not isinstance(error[1], FileNotFoundError):
            raise error[1]
    shutil.rmtree(path, onerror=missing_only)


def stage_program(source, destination, own=os.chown):
    destination.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    own(destination.parent, 0, 0)
    destination.parent.chmod(0o755)
    with source.open("rb") as incoming, destination.open("xb") as output:
        shutil.copyfileobj(incoming, output)
    destination.chmod(0o755)
    own(destination, 0, 0)
    info = destination.lstat()
    return dict(source=str(source), path=str(destination), uid=info.st_uid, gid=info.st_gid,
                mode=oct(info.st_mode & 0o7777), sha256=hashlib.sha256(destination.read_bytes()).hexdigest())


def archive_journal(source, destination):
    try:
        directory = os.open(source, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return None
    copied = []
    def copy_file(parent, name, output, label):
        try:
            fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        except FileNotFoundError:
            return
        with os.fdopen(fd, "rb") as incoming:
            if not stat.S_ISREG(os.fstat(incoming.fileno()).st_mode):
                raise RuntimeError(f"host journal entry is not regular: {label}")
            target = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
            with os.fdopen(target, "wb") as outgoing:
                shutil.copyfileobj(incoming, outgoing)
        copied.append(label)
    try:
        destination.mkdir(mode=0o700, parents=True, exist_ok=True)
        for name in ("journal.json", "journal.new"):
            copy_file(directory, name, destination / name, name)
        try:
            expired = os.open("outbox-expired", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
        except FileNotFoundError:
            expired = None
        if expired is not None:
            try:
                (destination / "outbox-expired").mkdir(mode=0o700, exist_ok=True)
                for name in os.listdir(expired):
                    if name.endswith(".json"):
                        copy_file(expired, name, destination / "outbox-expired" / name, "outbox-expired/" + name)
            finally:
                os.close(expired)
    finally:
        os.close(directory)
    return copied


def qualification_args(original):
    if not isinstance(original, str) or not original.strip() or any(c in original for c in "\r\n\0"):
        raise ValueError("invalid manifest boot_args")
    if "systemd.run=" in original or "motor-qualification.service" in original:
        raise ValueError("image already has qualification boot arguments")
    unit = base64.b64encode((HERE / "guest/motor-qualification.service").read_bytes()).decode()
    # Direct tty output preserves exact evidence lines; journald's console
    # forwarding can prepend an identifier and PID to the checker's stdout.
    console = base64.b64encode(b"[Service]\nStandardOutput=tty\nStandardError=tty\nTTYPath=/dev/ttyS0\n").decode()
    runner_console = base64.b64encode(b"[Service]\nStandardOutput=journal+console\nStandardError=journal+console\n").decode()
    return (original + " systemd.wants=motor-image-check.service systemd.wants=motor-qualification.service"
            + " systemd.set_credential_binary=systemd.extra-unit.motor-qualification.service:" + unit
            + " systemd.set_credential_binary=systemd.unit-dropin.motor-image-check.service:" + console
            + " systemd.set_credential_binary=systemd.unit-dropin.motor-tenant-runner.service:" + runner_console)


def exact_line(text, expected):
    # Serial lines end in CRLF. No timestamps, whitespace, prefixes or partial
    # success strings are stripped: those are not the checker's completion line.
    return expected in text.splitlines()


def guard(free, additional=0):
    if free < FLOOR + additional:
        raise RuntimeError(f"disk guard: {free} bytes free; need {FLOOR + additional} (170 GiB floor + allocation)")


def send_frame(conn, body):
    data = json.dumps(body, separators=(",", ":")).encode()
    conn.sendall(struct.pack(">I", len(data)) + data)


def exact(conn, size):
    data = bytearray()
    while len(data) < size:
        chunk = conn.recv(size - len(data))
        if not chunk:
            raise EOFError("connection closed before frame completed")
        data.extend(chunk)
    return bytes(data)


def frame(conn):
    size, = struct.unpack(">I", exact(conn, 4))
    if not 0 < size <= 16 * 1024**2:
        raise ValueError(f"invalid frame size: {size}")
    return json.loads(exact(conn, size))


# Upper bound on one job's whole exchange, from lease hello to its terminal
# frame. The first job also waits for tenant attachment (host deadline 60 s),
# and each job runs for at most its 30 s timeout; the rest is ample room for
# bundle upload, workspace setup and queueing.
JOB_DEADLINE_S = 600


def reply_frame(conn, deadline, read=frame):
    """The next frame that is not a heartbeat, read before deadline.

    The runner writes heartbeats on its own schedule, starting on its writer's
    first tick, so one can arrive before the reply to hello or any other
    request. Waiting for a specific reply therefore reads past heartbeats, but
    only until the monotonic deadline; each read is also bounded by the
    socket timeout.
    """
    while True:
        if time.monotonic() > deadline:
            raise TimeoutError("no reply frame before the deadline; only heartbeats arrived")
        item = read(conn)
        if item.get("type") != "heartbeat":
            return item


def job_id():
    value = (int(time.time() * 1000) << 80) | (7 << 76) | (int.from_bytes(os.urandom(2), "big") & 4095) << 64
    value |= (2 << 62) | (int.from_bytes(os.urandom(8), "big") & ((1 << 62) - 1))
    return str(uuid.UUID(int=value))


def run_request(identifier, commit, tree, bundle, command):
    return dict(type="run", job_id=identifier, workspace_key=WORKSPACE, repository_root=WORKSPACE,
                cwd=WORKSPACE, command=command, env={}, weight_hint=1, timeout=30, queue_wait_limit_s=30,
                snapshot_commit=commit, tree_hash=tree, integration_tip="", siblings=[],
                bundles=[dict(index=0, repository=WORKSPACE, length=len(bundle), sha256=hashlib.sha256(bundle).hexdigest())])


def parity_job_command(gnu_test_sha256, number):
    if not re.fullmatch(r'[a-f0-9]{64}', gnu_test_sha256):
        raise ValueError('image inventory lacks the package-qualified gnutest hash')
    return ("python3 toolchain-parity.py --check --role tenant --job "
            f"--gnu-test-sha256 {gnu_test_sha256} > \"$d/parity-job-{number}.json\" "
            f'|| {{ cat "$d/parity-job-{number}.json" >&2; exit 1; }}; ')


def javascript_job_command():
    """Fails the job unless the runner's workspace setup installed each
    fixture's locked local dependency without running its preinstall script."""
    return (f'for m in {" ".join(JAVASCRIPT_MANAGERS)}; do '
            'test -f "$m/node_modules/fixture-dep/index.js" '
            '|| { echo "runner setup did not install the $m fixture" >&2; exit 1; }; '
            'test ! -e "$m/lifecycle-ran" '
            '|| { echo "the $m fixture lifecycle script ran" >&2; exit 1; }; done; ')


def stage_javascript_fixtures(fixture, source=JAVASCRIPT_FIXTURES):
    for manager in JAVASCRIPT_MANAGERS:
        shutil.copytree(source / manager, fixture / manager, ignore=shutil.ignore_patterns("node_modules"))


def parity_inputs(image, repo=REPO):
    manifest = json.loads((image / 'manifest.json').read_text())
    inventory = json.loads((image / 'tools.json').read_text())['parity']
    for name in ('toolchain-parity.py', 'toolchain.json', 'cargo-seed.py', 'syscall-surface.py'):
        relative = 'script/runner/' + name
        actual = hashlib.sha256((repo / relative).read_bytes()).hexdigest()
        if actual != manifest['versions']['runner_inputs_sha256'][relative]:
            raise RuntimeError(f'qualification parity input differs from built image: {relative}')
    return inventory['observed']['tools']['gnutest']['sha256']


def helper_unit_command(unit, binary, config):
    return ["systemd-run", "--collect", "--unit", unit, "--property=Delegate=yes", "--property=DelegateSubgroup=helper",
            "--property=Requires=motor-vm-storage-private.service", "--property=After=motor-vm-storage-private.service",
            "--property=KillMode=control-group", binary, "--config", config]


def assert_job(frames, code, expected_stdout, expected_stderr):
    streams = {"stdout": bytearray(), "stderr": bytearray()}
    terminals = []
    for item in frames:
        if item["type"] == "output":
            streams[item["stream"]].extend(base64.b64decode(item["bytes"], validate=True))
        if item["type"] == "terminal":
            terminals.append(item)
    if len(terminals) != 1 or terminals[0]["outcome"] != {"type": "exit", "code": code}:
        raise RuntimeError(f"wrong terminal: {terminals}")
    out, err = bytes(streams["stdout"]), bytes(streams["stderr"])
    if not out.startswith(expected_stdout) or err != expected_stderr:
        raise RuntimeError(f"wrong output: stdout={out!r}, stderr={err!r}")
    match = re.fullmatch(rb"TARGET (/.+) ([0-9]+)\n", out[len(expected_stdout):])
    if match is None:
        raise RuntimeError(f"missing exact target path and inode: {out!r}")
    return match.groups()


class Harness:
    def __init__(self, args):
        self.args = args
        self.results = args.results.resolve()
        self.results.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.log = (self.results / "raw.jsonl").open("w")
        self.verdicts = {}
        self.name = "qualify-" + uuid.uuid4().hex[:12]
        self.work = ROOT / self.name
        self.binary_dir = self.work / "bin"
        self.image = ROOT / "tenant-images" / self.name
        self.home = ROOT / "tenants" / self.name
        self.registry = REGISTRY / (self.name + ".json")
        self.uid = pwd.getpwnam(args.host_user).pw_uid
        self.gid = pwd.getpwnam(args.host_user).pw_gid
        self.units = []
        self.processes = []
        self.started_helper = False
        self.owned = False
        self.bindings_before = None
        self.provisioner: Any = None
        self.base_jails_before: set[Path] = set()
        self.forwarding_before = ""

    def record(self, kind, **data):
        record = dict(kind=kind, **data)
        self.log.write(json.dumps(record) + "\n")
        self.log.flush()
        print(json.dumps(record), flush=True)

    def run(self, *args, input=None, allow_missing_unit=False, **kwargs):
        result = subprocess.run([str(a) for a in args], input=input, capture_output=True, timeout=600, **kwargs)
        self.record("command", argv=[str(a) for a in args], code=result.returncode,
                    stdout=result.stdout.decode(errors="replace"), stderr=result.stderr.decode(errors="replace"))
        missing = allow_missing_unit and str(args[0]) == "systemctl" and unit_not_loaded(result)
        if result.returncode and not missing:
            raise RuntimeError(f"command failed: {args[0]} (exit {result.returncode})")
        return result.stdout

    def step(self, name, action):
        start = time.monotonic()
        try:
            evidence = action()
        except BaseException as error:
            self.verdicts[name] = False
            self.record("FAIL", step=name, error=describe_error(error), **error_evidence(error), elapsed_s=time.monotonic() - start)
            self.failure_logs()
            raise
        self.verdicts[name] = True
        self.record("PASS", step=name, evidence=evidence, elapsed_s=time.monotonic() - start)
        return evidence

    def failure_logs(self):
        paths = list(self.results.glob("*.log"))
        boot = self.results / "base-boot"
        if boot.exists():
            paths.extend(path for path in boot.iterdir() if path.is_file())
        for path in sorted(paths):
            try:
                data = path.read_bytes()
                self.record("failure_artifact", path=str(path), bytes=len(data), tail=data[-65536:].decode(errors="replace"))
            except OSError as error:
                self.record("failure_artifact_error", path=str(path), error=describe_error(error))

    def wait(self, predicate, description, timeout=180):
        deadline = time.monotonic() + timeout
        while True:
            value = predicate()
            if value:
                return value
            if time.monotonic() > deadline:
                raise TimeoutError(description)
            time.sleep(.1)

    def console(self):
        chunks = []
        for path in (self.results / "base-console.log", self.home / "console.log"):
            try:
                chunks.append(path.read_text(errors="replace"))
            except FileNotFoundError:
                pass
        return "\n".join(chunks)

    def expect_console(self, line):
        def check():
            output = self.console()
            if "FAIL guest qualification:" in output:
                raise RuntimeError(output)
            return output if exact_line(output, line) else None
        return self.wait(check, f"console never contained exact line {line!r}", 240)

    def helper(self, op):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(180)
            try:
                os.setegid(self.gid)
                os.seteuid(self.uid)
                conn.connect(str(HELPER_SOCKET))
            finally:
                os.seteuid(0)
                os.setegid(0)
            conn.sendall(json.dumps(dict(op=op, tenant=self.name)).encode() + b"\n")
            data = b""
            while not data.endswith(b"\n"):
                data += conn.recv(4096) or (_ for _ in ()).throw(EOFError())
            reply = json.loads(data)
        self.record("helper", op=op, reply=reply)
        if reply["result"] != "tenant":
            raise RuntimeError(f"helper refused {op}: {reply}")
        return reply

    def json_file(self, path, body, mode=0o600):
        path.write_text(json.dumps(body, indent=2) + "\n")
        path.chmod(mode)

    def wait_for_helper(self, unit):
        def ready():
            state = self.run("systemctl", "show", unit, "-p", "LoadState", "-p", "ActiveState", "-p", "SubState",
                             "-p", "Result", "-p", "ExecMainStatus", allow_missing_unit=True).decode()
            if not state.strip() or any(setting in state.splitlines() for setting in ("ActiveState=failed", "ActiveState=inactive", "LoadState=not-found")):
                journal = self.run("journalctl", "-u", unit, "--no-pager", "-n", "80").decode()
                raise RuntimeError(f"helper exited before listening: {state}; journal: {journal}")
            if HELPER_SOCKET.exists():
                try:
                    self.helper("adopt")
                    return True
                except (FileNotFoundError, ConnectionRefusedError, PermissionError):
                    pass
            return False
        self.wait(ready, "helper listener did not appear", 30)

    def launch(self, binary, *args, host_user=False):
        output = (self.results / (Path(binary).name + ".log")).open("ab", buffering=0)
        process = subprocess.Popen([str(binary), *map(str, args)], stdout=output, stderr=output,
                                   cwd="/", user=self.uid if host_user else None,
                                   group=self.gid if host_user else None, extra_groups=[] if host_user else None)
        self.processes.append(process)
        output.close()
        return process

    def key(self, name):
        path = self.work / (name + ".pem")
        self.run("openssl", "genpkey", "-algorithm", "ED25519", "-out", path)
        public = self.run("openssl", "pkey", "-in", path, "-pubout", "-outform", "DER")[-32:]
        # DER private output contains the raw seed. It is intentionally never logged.
        der = subprocess.check_output(["openssl", "pkey", "-in", str(path), "-outform", "DER"])
        return path, der[-32:], dict(kty="OKP", crv="Ed25519", x=b64(public))

    def preflight(self):
        if sys.platform != "linux" or os.geteuid() != 0 or self.uid == 0:
            raise RuntimeError("requires Linux root and a non-root --host-user")
        self.record("vm_storage_propagation", evidence=require_private_view(ROOT))
        guard(shutil.disk_usage(ROOT).free, 54 * GIB)
        if HELPER_SOCKET.exists() or HELPER_SOCKET.is_symlink() or any(REGISTRY.glob("*.json")):
            raise RuntimeError("refusing an existing helper socket or registered tenants; never interrupt a live helper")
        if (ROOT / "jail/tenants").exists() and any((ROOT / "jail/tenants").iterdir()):
            raise RuntimeError("pre-existing tenant jails require operator investigation")
        for device in ("/dev/kvm", "/dev/net/tun"):
            if not Path(device).exists():
                raise RuntimeError(f"missing {device}")
        for name in ("motor-vm-helper", "motor-host", "motor-fake-broker"):
            path = self.args.bin_dir.resolve() / name
            if not path.is_file() or not os.access(path, os.X_OK):
                raise RuntimeError(f"missing executable {path}; build the current checkout first")
        self.run("python3", "--version")
        self.run("systemctl", "--version")
        self.run("openssl", "version")
        self.run("/usr/local/lib/firecracker/1.17.0/firecracker", "--version")
        self.run("/usr/local/lib/firecracker/1.17.0/jailer", "--version")
        listing = self.run(self.args.runner_test_binary.resolve(), "--list", "--format", "terse").decode()
        if f"{TEST}: test" not in listing.splitlines():
            raise RuntimeError("supplied binary does not contain the required ignored test; not skipped")
        source = self.args.image.resolve()
        if source.parent != ROOT / "tenant-images":
            raise RuntimeError("image must be under /var/lib/motor-vm/tenant-images")
        self.run("python3", HERE / "build.py", "inventory", source)
        self.provisioner = module("tenant_provision", HERE.parent / "tenant-provision/provision.py")
        self.provisioner.trusted(source / "manifest.json")
        for directory in (ROOT, REGISTRY, BINDINGS.parent, ROOT / "tenants", ROOT / "jail"):
            directory.mkdir(parents=True, exist_ok=True)
            self.provisioner.trusted(directory)
        alias_output = self.run("unshare", "--mount", "--propagation", "private", "--", "python3", HERE / "test_mount_alias_root.py").decode()
        if not exact_line(alias_output, "PASS real bind-alias mount fence: 2 checks passed"):
            raise RuntimeError("real bind-alias fixture did not report both checks; no skip is a pass")
        baseline = self.host_state()
        if baseline["netns"] or baseline["tenant_taps"] or baseline["tenant_table"] or baseline["tenant_vmm"]:
            raise RuntimeError(f"pre-existing tenant/provision resources require investigation: {baseline}")
        self.baseline = baseline
        self.forwarding_before = FORWARDING.read_text()
        self.base_jails_before = set((ROOT / "jail").glob("provision-*"))
        self.bindings_before = BINDINGS.read_bytes() if BINDINGS.exists() else None
        if self.bindings_before is not None and json.loads(self.bindings_before):
            raise RuntimeError("refusing existing host tenant bindings")
        return dict(free_bytes=shutil.disk_usage(ROOT).free, baseline=baseline,
                    test_binary=str(self.args.runner_test_binary.resolve()))

    def host_state(self) -> dict[str, Any]:
        links = json.loads(self.run("ip", "-j", "link", "show"))
        nft = json.loads(self.run("nft", "-j", "list", "tables"))
        failed = self.run("systemctl", "--failed", "--no-legend", "--plain", "--no-pager").decode()
        processes = self.run("ps", "-eo", "pid=,args=").decode()
        netns = self.run("ip", "netns", "list").decode()
        return dict(tenant_taps=[v["ifname"] for v in links if v["ifname"].startswith("mtap")],
                    tenant_table=any(v.get("table", {}).get("name") == "motor_tenant" for v in nft["nftables"]),
                    failed_units=failed.splitlines(),
                    netns=[line for line in netns.splitlines() if line.startswith("base-")],
                    ipv4_forwarding=FORWARDING.read_text().strip(),
                    tenant_vmm=[line for line in processes.splitlines()
                                if re.search(r"--id[ =](tenant-[0-9]+|base-[0-9a-f]+)(?:\s|$)", line)])

    def provision(self):
        self.work.mkdir(mode=0o755)
        self.owned = True
        for name in ("motor-vm-helper", "motor-host", "motor-fake-broker"):
            self.record("staged_program", **stage_program(self.args.bin_dir.resolve() / name, self.binary_dir / name))
        self.record("staged_program", **stage_program(Path("/usr/bin/true"), self.binary_dir / "unused-job-loader"))
        self.image.mkdir(mode=0o755)
        source = self.args.image.resolve()
        self.gnu_test_sha256 = parity_inputs(source)
        manifest = json.loads((source / "manifest.json").read_text())
        original = dict(manifest)
        manifest.update(version=self.name, boot_args=qualification_args(manifest["boot_args"]))
        self.json_file(self.image / "manifest.json", manifest, 0o444)
        # Hard links preserve the exact qualified kernel/rootfs bytes without
        # copying or editing either immutable release artifact.
        for name in ("rootfs", "vmlinux"):
            os.link(source / name, self.image / name)
        self.json_file(self.results / "input-manifest.json", original)
        self.json_file(self.results / "qualification-manifest.json", manifest)
        self.config = self.provisioner.checked(dict(
            tenant=self.name, vm=self.name, sub="acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", image_version=self.name,
            vcpu_ceiling=2, guest_ram_mib=16384, disk_quota_bytes=20 * GIB, cpuset=self.args.cpuset,
            tap_index=self.args.tap_index, idle_suspend_s=3600, host_floor_bytes=FLOOR,
            memory_headroom_mib=512, snapshot_memory_high_mib=14336, io_weight=10,
            resolvers=["1.1.1.1"], capture_console=True))
        self.json_file(self.results / "provision-config.json", self.config)

        def stage(mount):
            dest = mount / "qualification"
            dest.mkdir(mode=0o700)
            shutil.copyfile(HERE / "guest/hosted_qualify.py", dest / "run.py")
            # The guest checks ownership with this checkout's rule, not the image's copy.
            shutil.copyfile(HERE / "guest/root_paths.py", dest / "root_paths.py")
            for name in ("toolchain-parity.py", "toolchain.json", "cargo-seed.py", "syscall-surface.py"):
                shutil.copyfile(REPO / "script/runner" / name, dest / name)
            (dest / "gnutest.sha256").write_text(self.gnu_test_sha256)
            shutil.copyfile(self.args.runner_test_binary.resolve(), dest / "runner-tests")
            (dest / "runner-tests").chmod(0o700)

        with (self.results / "base-console.log").open("ab", buffering=0) as console:
            def base(c, image, path):
                self.provisioner.build_base(c, image, path, console=console, diagnostics=self.results / "base-boot")
            self.provisioner.provision(self.config, make_base=base, stage=stage)
        # The helper has one inert job slot because its existing configuration
        # requires one. Only persistent-tenant operations are sent by this lane.
        (self.work / "unused-job-snapshots").mkdir(mode=0o700)
        snapshot = {name: str(self.work / "unused-job-snapshots" / name) for name in ("vmstate", "memory", "rootfs", "record")}
        helper_config = dict(allowed_uids=[self.uid], socket_gid=self.gid, uid_base=290000,
                             job_account_range=dict(base=300000, count=1),
                             slots=[dict(cpus=self.args.cpuset, mems="0", cpu_max="100000 100000",
                                         memory_max=GIB, io_weight=10, io_max=None)],
                             jailer="/usr/local/lib/firecracker/1.17.0/jailer",
                             firecracker="/usr/local/lib/firecracker/1.17.0/firecracker", loader=str(self.binary_dir / "unused-job-loader"),
                             job_snapshot=snapshot, scan_snapshot=snapshot, hugepages_per_slot=1,
                             agent_port=52, proxy_port=53)
        self.json_file(self.work / "helper.json", helper_config)
        os.chown(self.work / "helper.json", 0, 0)
        self.json_file(self.results / "helper-config.json", helper_config)
        unit = self.name + "-helper.service"
        self.units.append(unit)
        self.started_helper = True
        self.run(*helper_unit_command(unit, self.binary_dir / "motor-vm-helper", self.work / "helper.json"))
        self.wait_for_helper(unit)
        self.start_admission()
        # Admission itself does the first ensure, disk substitution and attachment.
        # This job also proves real output and an intentional nonzero exit return.
        self.first_frames = self.job(1)
        return dict(tenant=self.name, original_image=str(source), kernel_sha256=manifest["kernel_sha256"],
                    rootfs_sha256=manifest["rootfs_sha256"])

    def start_admission(self):
        _, broker_seed, _ = self.key("broker")
        _, host_seed, host_jwk = self.key("host")
        self.requester, _, self.requester_jwk = self.key("requester")
        self.run("openssl", "req", "-x509", "-newkey", "ed25519", "-nodes", "-days", "1",
                 "-subj", "/CN=qualification.invalid", "-keyout", self.work / "tls.key", "-out", self.work / "tls.pem")
        public = self.run("openssl", "x509", "-in", self.work / "tls.pem", "-pubkey", "-noout")
        spki = self.run("openssl", "pkey", "-pubin", "-outform", "DER", input=public)
        self.pin = hashlib.sha256(spki).hexdigest()
        self.port = free_port()
        self.broker_port = free_port()
        self.bearer = b64(os.urandom(32))
        self.salt = os.urandom(32)
        broker = dict(worker_bearer=self.bearer, subject=self.config["sub"], placement=dict(host=self.name, vm=self.name),
                      endpoint=f"127.0.0.1:{self.port}", endpoint_spki_sha256=self.pin, kid="qualification",
                      signing_seed=b64(broker_seed), registered_hosts=[dict(host=self.name, public_key=host_jwk)])
        self.json_file(self.work / "broker.json", broker)
        self.launch(self.binary_dir / "motor-fake-broker", "--runner-instance", self.name,
                    "--config", self.work / "broker.json", "--listen", f"127.0.0.1:{self.broker_port}", "--max-s", "30")
        self.wait(lambda: listening(self.broker_port), "fake broker did not start", 30)
        private = self.work / "host-private"
        private.mkdir(mode=0o700)
        os.chown(private, self.uid, self.gid)
        for path in (self.work / "tls.key", self.work / "tls.pem"):
            os.chown(path, self.uid, self.gid)
        (private / "seed").write_bytes(host_seed)
        (private / "seed").chmod(0o600)
        os.chown(private / "seed", self.uid, self.gid)
        self.json_file(self.work / "bindings.json", {self.name: dict(tenant=self.name, sub=self.config["sub"])}, 0o644)
        config = dict(host_id=self.name, listen=f"127.0.0.1:{self.port}", certificate=str(self.work / "tls.pem"),
                      private_key=str(self.work / "tls.key"), host_signing_key=str(private / "seed"),
                      journal_dir=str(private / "journal"), helper_socket=str(HELPER_SOCKET), registry_dir=str(REGISTRY),
                      bindings=str(self.work / "bindings.json"), resolvers=self.config["resolvers"],
                      admin_socket=str(private / "admin.sock"), storage=str(ROOT),
                      fake_broker_url=f"http://127.0.0.1:{self.broker_port}")
        self.json_file(self.work / "host.json", config, 0o644)
        self.launch(self.binary_dir / "motor-host", self.work / "host.json", host_user=True)
        self.wait(lambda: listening(self.port), "host admission did not start", 30)

    def job(self, number):
        identifier = job_id()
        fixture = self.work / "fixture"
        if not fixture.exists():
            fixture.mkdir()
            self.run("git", "init", "--quiet", fixture)
            (fixture / "input").write_text("tenant image qualification\n")
            for name in ("toolchain-parity.py", "toolchain.json", "cargo-seed.py", "syscall-surface.py"):
                shutil.copyfile(REPO / "script/runner" / name, fixture / name)
            stage_javascript_fixtures(fixture)
            self.run("git", "-C", fixture, "add", ".")
            self.run("git", "-C", fixture, "-c", "user.name=Qualification", "-c", "user.email=qualification@example.invalid",
                     "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture")
        commit = self.run("git", "-C", fixture, "rev-parse", "HEAD").decode().strip()
        tree = self.run("git", "-C", fixture, "rev-parse", "HEAD^{tree}").decode().strip()
        reference = f"refs/ck-motor/{identifier}/snapshot"
        self.run("git", "-C", fixture, "update-ref", reference, commit)
        bundle_path = fixture / "input.bundle"
        self.run("git", "-C", fixture, "bundle", "create", bundle_path, reference)
        self.run("git", "-C", fixture, "update-ref", "-d", reference)
        bundle = bundle_path.read_bytes()
        lease_body = dict(job_id=identifier, vcpus=1, platform="linux-x64", net=False,
                          workspace_key_hash=b64(hmac.digest(self.salt, ("ck-ws-v1\n" + WORKSPACE).encode(), "sha256")),
                          repo_hash=b64(hmac.digest(self.salt, ("ck-repo-v1\n" + WORKSPACE).encode(), "sha256")),
                           placement_hint=None, avoid_host=None, timeout_s=180, requester_jwk=self.requester_jwk)
        request = urllib.request.Request(f"http://127.0.0.1:{self.broker_port}/v1/leases",
                                         data=json.dumps(lease_body).encode(),
                                         headers={"Authorization": "Bearer " + self.bearer, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=30) as response:
            lease = json.load(response)
        if lease["endpoint"] != f"127.0.0.1:{self.port}" or lease["placement"] != dict(host=self.name, vm=self.name):
            raise RuntimeError("fake broker returned an unexpected endpoint or placement")
        self.record("lease", job_id=identifier, placement=lease["placement"], max_s=lease["max_s"])
        command = 'set -eu; d="${CARGO_TARGET_DIR:?}"; mkdir -p "$d"; '
        command += parity_job_command(self.gnu_test_sha256, number)
        command += javascript_job_command()
        if number == 1:
            command += f'printf "%s" {self.name} > "$d/qualification-sentinel"; '
        else:
            command += f'test "$(cat "$d/qualification-sentinel")" = {self.name}; '
        if number == 3:
            command += 'touch qualification-finish; '
        command += (f"printf 'QUALIFY_STDOUT_{number}\\n'; printf 'QUALIFY_STDERR_{number}\\n' >&2; "
                    'printf "TARGET %s %s\\n" "$d" "$(stat -c %i "$d")"; ' + ("exit 23" if number == 1 else "exit 0"))
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        with socket.create_connection(("127.0.0.1", self.port), timeout=180) as tcp, context.wrap_socket(tcp) as conn:
            cert_bytes = conn.getpeercert(binary_form=True)
            if not cert_bytes:
                raise RuntimeError("TLS peer supplied no certificate")
            cert = ssl.DER_cert_to_PEM_cert(cert_bytes).encode()
            public = self.run("openssl", "x509", "-pubkey", "-noout", input=cert)
            spki = self.run("openssl", "pkey", "-pubin", "-outform", "DER", input=public)
            if hashlib.sha256(spki).hexdigest() != lease["endpoint_spki_sha256"]:
                raise RuntimeError("host TLS SPKI did not match the fake broker lease")
            send_frame(conn, dict(type="lease_hello", v=1, token=lease["job_token"], requester_jwk=self.requester_jwk))
            return self.converse(conn, identifier, lease, run_request(identifier, commit, tree, bundle, command), bundle)

    def sign(self, signing_input):
        message = self.work / "challenge"
        message.write_bytes(signing_input)
        return subprocess.check_output(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", str(self.requester), "-in", str(message)], timeout=30)

    def converse(self, conn, identifier, lease, request, bundle):
        """Admission challenge, runner hello, Run, bundle upload and output frames."""
        deadline = time.monotonic() + JOB_DEADLINE_S
        challenge = reply_frame(conn, deadline)
        if challenge["type"] != "challenge" or challenge["host_id"] != self.name:
            raise RuntimeError(f"host challenge refused: {challenge}")
        signing_input = (f"motor-host-challenge-v1\n{challenge['nonce']}\n{self.name}\n{identifier}\n{challenge['ts']}\n").encode()
        send_frame(conn, dict(type="challenge_response", sig=b64(self.sign(signing_input))))
        reply = reply_frame(conn, deadline)
        self.record("lease_admission", reply=reply)
        if reply["type"] != "lease_ok":
            raise RuntimeError(f"host refused lease: {reply}")
        send_frame(conn, dict(type="hello", protocol_version=1, started=True, default_branch_tip=True))
        reply = reply_frame(conn, deadline)
        if reply.get("type") != "hello" or reply.get("protocol_version") != 1:
            raise RuntimeError(f"runner hello failed: {reply}")
        send_frame(conn, request)
        reply = reply_frame(conn, deadline)
        self.record("job_frame", job_id=identifier, frame=reply)
        if reply["type"] != "accepted":
            raise RuntimeError(f"runner refused Run: {reply}")
        for offset in range(0, len(bundle), 65536):
            send_frame(conn, dict(type="bundle_chunk", index=0, offset=offset,
                                 data=base64.b64encode(bundle[offset:offset + 65536]).decode()))
        frames = []
        while True:
            reply = reply_frame(conn, deadline)
            self.record("job_frame", job_id=identifier, frame=reply)
            frames.append(reply)
            if reply["type"] in ("refusal", "lease_refused"):
                raise RuntimeError(f"job refused: {reply}")
            if reply["type"] == "hosted_terminal":
                return frames

    def stop_processes(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)

    def archive_host_state(self, phase):
        copied = archive_journal(self.work / "host-private/journal", self.results / "host-state" / phase)
        self.record("host_state_archive" if copied is not None else "host_state_absent", phase=phase, files=copied)
        config = self.work / "host.json"
        try:
            shutil.copyfile(config, self.results / "host-config.json")
        except FileNotFoundError:
            pass

    def teardown(self):
        errors = []

        def attempt(action):
            try:
                action()
            except Exception as error:
                errors.append(describe_error(error))

        attempt(lambda: self.archive_host_state("before-stop"))
        attempt(self.stop_processes)
        attempt(lambda: self.archive_host_state("after-host-stop"))
        if self.started_helper and self.registry.exists() and HELPER_SOCKET.exists():
            try:
                self.helper("stop_tenant")
            except (FileNotFoundError, ConnectionRefusedError):
                self.record("helper_already_gone", tenant=self.name)
            except Exception as error:
                errors.append(describe_error(error))
        for unit in reversed(self.units):
            attempt(lambda unit=unit: self.run("systemctl", "show", unit, "-p", "ActiveState", "-p", "SubState",
                                               "-p", "Result", "-p", "ExecMainCode", "-p", "ExecMainStatus", "-p", "ControlGroup", allow_missing_unit=True))
            attempt(lambda unit=unit: self.run("journalctl", "-u", unit, "--no-pager", "--output=short-monotonic"))
            attempt(lambda unit=unit: self.run("systemctl", "stop", unit, allow_missing_unit=True))
            def save_journal(unit=unit):
                output = self.run("journalctl", "-u", unit, "--no-pager", "--output=short-monotonic")
                (self.results / (unit + ".journal.log")).write_bytes(output)
            attempt(save_journal)
            # Reset only this invocation's failed unit, never unrelated failures.
            attempt(lambda unit=unit: self.run("systemctl", "reset-failed", unit, allow_missing_unit=True))
        if self.owned:
            (self.results / "console.log").write_text(self.console())
            mounts = MOUNTINFO.read_text()
            owned_paths = [self.work, self.home, self.image, ROOT / "jail/tenants" / self.name]
            extra_jails = set((ROOT / "jail").glob("provision-*")) - self.base_jails_before
            if extra_jails:
                errors.append(f"base provisioning jails remain: {sorted(map(str, extra_jails))}")
                owned_paths.extend(sorted(extra_jails))
            mounted = sorted({str(point) for path in owned_paths if path.exists()
                              for _, point in mounts_under(path, mounts)})
            self.record("teardown_mounts", remaining=mounted)
            if (self.home / "disk.img").exists():
                loops = self.run("losetup", "-j", self.home / "disk.img").decode().strip()
                if loops:
                    mounted.append(loops)
            if mounted:
                errors.append(f"refusing deletion with live mounts: {mounted}")
            others = [p for p in REGISTRY.glob("*.json") if p != self.registry]
            if others:
                errors.append(f"concurrent tenant registration: {others}; refusing global firewall cleanup")
            state = self.host_state()
            if state["tenant_vmm"]:
                errors.append(f"VMMs remain: {state['tenant_vmm']}")
            if not mounted and not state["tenant_vmm"] and not others:
                tap = f"mtap{self.args.tap_index}"
                if tap in state["tenant_taps"]:
                    attempt(lambda: self.run("ip", "link", "delete", tap))
                if state["tenant_table"]:
                    attempt(lambda: self.run("nft", "delete", "table", "inet", "motor_tenant"))
                self.registry.unlink(missing_ok=True)
                if BINDINGS.exists():
                    try:
                        current = json.loads(BINDINGS.read_text())
                    except FileNotFoundError:
                        current = {}
                    expected = {self.name: dict(tenant=self.name, sub=self.config["sub"])} if hasattr(self, "config") else {}
                    if current not in ({}, expected):
                        errors.append("host bindings changed concurrently; refusing to overwrite")
                    elif self.bindings_before is None:
                        BINDINGS.unlink()
                    else:
                        BINDINGS.write_bytes(self.bindings_before)
                        BINDINGS.chmod(0o644)
                for path in owned_paths:
                    if path.exists():
                        remove_owned_tree(path)
                if self.started_helper:
                    HELPER_SOCKET.unlink(missing_ok=True)
                FORWARDING.write_text(self.forwarding_before)
        after = self.host_state()
        self.record("teardown_state", state=after)
        for key in ("tenant_vmm", "tenant_taps", "tenant_table", "netns"):
            if after[key]:
                errors.append(f"leftover {key}: {after[key]}")
        baseline_failed = getattr(self, "baseline", {}).get("failed_units", [])
        baseline_names = {line.split()[0] for line in baseline_failed if line.split()}
        unrelated = []
        for line in after["failed_units"]:
            name = line.split()[0]
            if name in baseline_names:
                continue
            group = self.run("systemctl", "show", name, "-p", "ControlGroup", "--value", allow_missing_unit=True).decode().strip()
            if owned_unit(name, group, self.units, self.name):
                errors.append(f"owned failed host unit: {line}")
            else:
                unrelated.append(line)
        self.record("unrelated_new_failed_units", units=unrelated)
        if self.owned and after["ipv4_forwarding"] != self.forwarding_before.strip():
            errors.append("host IPv4 forwarding was not restored")
        if self.owned:
            for path in (self.work, self.home, self.image, ROOT / "jail/tenants" / self.name, self.registry):
                if path.exists():
                    errors.append(f"leftover {path}")
        if errors:
            raise RuntimeError("; ".join(errors))
        return after

    def execute(self):
        error = None
        try:
            self.step("preflight", self.preflight)
            self.step("provision and boot", self.provision)
            self.step("live inventory", lambda: self.expect_console(LIVE))
            self.step("tenant inventory", lambda: self.expect_console(TENANT))
            self.step("name service", lambda: self.expect_console(NAME_SERVICE))
            self.step("tool trees", lambda: self.expect_console(TOOL_TREES))
            self.step("root paths", lambda: self.expect_console(ROOT_PATHS))
            self.step("toolchain parity", lambda: self.expect_console(PARITY))
            self.step("javascript setup", lambda: self.expect_console(JAVASCRIPT_SETUP))
            self.step("vsock isolation", lambda: self.expect_console("PASS guest vsock isolation: 1 passed; 0 failed; 0 ignored"))
            def result(frames, number):
                return assert_job(frames, 23 if number == 1 else 0,
                                  f"QUALIFY_STDOUT_{number}\n".encode(), f"QUALIFY_STDERR_{number}\n".encode())
            first = self.step("first job", lambda: [v.decode() for v in result(self.first_frames, 1)])
            def warm():
                second = [v.decode() for v in result(self.job(2), 2)]
                if second != first:
                    raise RuntimeError(f"target directory was not reused: first={first}, second={second}")
                return second
            self.step("warm target reuse", warm)
            # Quiescing, freeze/thaw, memory.high and durable snapshots are owned
            # by the real helper. Never replace them with direct Firecracker APIs.
            def suspend():
                guard(shutil.disk_usage(ROOT).free, 16 * GIB)
                reply = self.helper("suspend")
                if reply["running"] is not False:
                    raise RuntimeError(f"suspend did not stop the VM: {reply}")
                return reply
            self.step("suspend", suspend)
            def restore():
                reply = self.helper("restore")
                if reply["running"] is not True or reply["restored"] is not True:
                    raise RuntimeError(f"restore fell back to a fresh base: {reply}")
                return reply
            self.step("restore", restore)
            def restored_job():
                restored = [v.decode() for v in result(self.job(3), 3)]
                if restored != first:
                    raise RuntimeError(f"restored target changed: {restored} != {first}")
                return restored
            self.step("post-restore job", restored_job)
            self.step("guest units", lambda: self.expect_console("PASS guest failed units: none"))
        except BaseException as caught:
            error = describe_error(caught)
        finally:
            try:
                self.step("teardown", self.teardown)
            except BaseException as caught:
                error = f"{error or ''}; teardown: {describe_error(caught)}"
            for name in STEPS:
                if name not in self.verdicts:
                    self.verdicts[name] = False
                    self.record("FAIL", step=name, error="not reached; never counted as passing")
            passed = all(self.verdicts.values()) and error is None
            summary = dict(verdict="PASS" if passed else "FAIL", steps=self.verdicts, error=error,
                           results=str(self.results), tenant=self.name)
            self.json_file(self.results / "summary.json", summary)
            self.record(summary["verdict"], step="qualification", evidence=summary)
            self.log.close()
        return 0 if passed else 1


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def listening(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=.1):
            return True
    except OSError:
        return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--runner-test-binary", type=Path, required=True)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--host-user", default="nobody")
    parser.add_argument("--cpuset", required=True, help="two available host CPUs, excluding reserved Windows CPUs")
    parser.add_argument("--tap-index", type=int, default=254)
    args = parser.parse_args()
    if sys.platform != "linux" or os.geteuid() != 0:
        parser.error("operator-only: run as root on the dedicated Linux KVM host, never through runon")
    os.umask(0o022)
    for key in list(os.environ):
        if key.startswith("GIT_CONFIG_"):
            del os.environ[key]
    def interrupted(signum, _frame):
        raise RuntimeError(f"interrupted by signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    # One operator owns the fixed helper socket and provisioning registry for the
    # entire run, including teardown. Existing tenant state is always refused.
    with (ROOT / "provision.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return Harness(args).execute()


if __name__ == "__main__":
    sys.exit(main())
