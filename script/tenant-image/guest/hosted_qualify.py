#!/usr/bin/python3
"""Root qualification payload, staged only on a disposable tenant disk."""
import os
import importlib.util
import shutil
import json
from pathlib import Path
import socket
import stat
import struct
import subprocess
import threading
import time
from typing import Any

TEST = "isolation::tests::hosted_job_cannot_open_vsock"
NAME_SERVICE = ("PASS guest name service: no VM nameserver, host name from files, "
                "sudo and lookup fast with an unreachable resolver, runner ready within 10 s")
# The resolver an earlier attach wrote into the VM. The VM itself has no route
# to it, which is exactly what made each sudo call wait out DNS timeouts.
UNREACHABLE_RESOLVER = "1.1.1.1"
FAST_S = 3.0
READY_S = 10.0
# Longer than the host's own attach deadline, so a slow attach is reported
# with the measured readiness time rather than a missing marker.
ATTACH_WAIT_S = 90.0
TOOL_ROOTS = (Path("/usr/local/lib/motor/rustup"), Path("/usr/local/lib/motor/cargo"))
TOOL_TREES = "PASS guest tool trees: root-owned and world-readable without ACLs; no copy-up at runner start"
ROOT_PATHS = "PASS guest root paths: everything root executes from is root-owned without group or other write"
PARITY = "PASS guest toolchain parity: tenant host contract and package-qualified gnutest"
JAVASCRIPT_SETUP = ("PASS guest runner JavaScript setup: bun.lock, package-lock.json and pnpm-lock.yaml "
                    "installed with the tenant runner's tools after attach")
RUNNER_JAVASCRIPT_PASS = "PASS runner JavaScript setup: bun.lock, package-lock.json and pnpm-lock.yaml installs"
SAMPLE_BYTES = 32 * 1024 * 1024


def nameservers(text):
    return [line.split()[1] for line in text.splitlines()
            if len(line.split()) >= 2 and line.split()[0] == "nameserver"]


def runner_ready_seconds(start_monotonic_us, ready_realtime_s, now_realtime_s, now_monotonic_s):
    """Seconds from the runner unit's start to the attach completion marker.

    systemd reports the start on the monotonic clock and the marker's mtime is
    on the wall clock; both are converted with the clocks' current offset.
    """
    if start_monotonic_us <= 0:
        raise ValueError("tenant runner has no start timestamp")
    started = start_monotonic_us / 1e6 + (now_realtime_s - now_monotonic_s)
    return ready_realtime_s - started


def timed(*args):
    start = time.monotonic()
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60)
    return result.returncode, result.stdout, time.monotonic() - start


def name_service_probe():
    resolv = Path("/etc/resolv.conf")
    original = resolv.read_text()
    print(f"guest resolv.conf: {original!r}", flush=True)
    if nameservers(original):
        raise RuntimeError(f"the VM's own resolv.conf names resolvers: {nameservers(original)}")
    host = socket.gethostname()
    hosts = Path("/etc/hosts").read_text()
    print(f"guest hostname {host!r}; /etc/hosts: {hosts!r}", flush=True)
    if not any(host in line.split()[1:] for line in hosts.splitlines() if not line.startswith("#")):
        raise RuntimeError(f"/etc/hosts does not map the host name {host}")
    try:
        resolv.write_text(f"nameserver {UNREACHABLE_RESOLVER}\n")
        lookup = timed("getent", "ahosts", host)
        sudo = timed("sudo", "-n", "true")
    finally:
        resolv.write_text(original)
    for name, (code, output, elapsed) in (("getent ahosts", lookup), ("sudo -n true", sudo)):
        print(f"{name} with nameserver {UNREACHABLE_RESOLVER}: exit={code} elapsed_s={elapsed:.3f} "
              f"output={output!r}", flush=True)
        if code or elapsed >= FAST_S:
            raise RuntimeError(f"{name} took {elapsed:.3f} s (exit {code}) with an unreachable resolver")
    start = int(subprocess.run(["systemctl", "show", "-P", "ExecMainStartTimestampMonotonic",
                                "motor-tenant-runner.service"], stdout=subprocess.PIPE, text=True,
                               check=True).stdout.strip() or 0)
    marker = Path("/run/motor-tenant-generation")
    deadline = time.monotonic() + ATTACH_WAIT_S
    while not marker.exists():
        if time.monotonic() > deadline:
            raise RuntimeError(f"attach did not complete within {ATTACH_WAIT_S:.0f} s of this probe")
        time.sleep(.1)
    ready = runner_ready_seconds(start, marker.stat().st_mtime,
                                 time.clock_gettime(time.CLOCK_REALTIME), time.monotonic())
    print(f"tenant runner start to attach completion: {ready:.3f} s", flush=True)
    if not 0 <= ready < READY_S:
        raise RuntimeError(f"tenant runner took {ready:.3f} s to become ready")
    print(NAME_SERVICE, flush=True)


def tree_inventory(root):
    """Entries, regular-file bytes and entries not plainly world-readable."""
    entries, size, closed = 0, 0, []
    for path in [root, *(Path(d, n) for d, ds, fs in os.walk(root) for n in ds + fs)]:
        metadata = path.lstat()
        entries += 1
        if stat.S_ISLNK(metadata.st_mode):
            continue
        if stat.S_ISREG(metadata.st_mode):
            size += metadata.st_size
        needed = 0o055 if stat.S_ISDIR(metadata.st_mode) else 0o044
        has_acl = "system.posix_acl_access" in os.listxattr(path, follow_symlinks=False)
        if metadata.st_uid or metadata.st_gid or metadata.st_mode & needed != needed or has_acl:
            closed.append(str(path))
    return entries, size, closed


def upper_bytes():
    usage = os.statvfs("/overlay")
    return (usage.f_blocks - usage.f_bfree) * usage.f_frsize


def tool_tree_probe():
    # Runner startup must not have written to the image toolchain: any such
    # write appears as a copied-up path in the RAM overlay's upper directory.
    copied = [str(path) for path in (Path("/overlay/upper") / root.relative_to("/") for root in TOOL_ROOTS)
              if path.exists()]
    measurement = dict(copied_up_at_start=copied, overlay_upper_bytes=upper_bytes())
    metacopy = Path("/sys/module/overlay/parameters/metacopy")
    measurement["overlay_metacopy"] = metacopy.read_text().strip() if metacopy.exists() else None
    closed = []
    for root in TOOL_ROOTS:
        start = time.monotonic()
        entries, size, unreadable = tree_inventory(root)
        measurement[str(root)] = dict(entries=entries, file_bytes=size, not_world_readable=len(unreadable),
                                      stat_walk_s=round(time.monotonic() - start, 3))
        closed += unreadable[:5]
    # Time an ACL grant over a bounded sample of the toolchain's files, the
    # same operation the runner used to apply to the whole tree on each start.
    sample, sample_bytes = [], 0
    for directory, _, files in os.walk(TOOL_ROOTS[0]):
        for name in files:
            path = Path(directory, name)
            if path.is_symlink() or sample_bytes >= SAMPLE_BYTES:
                continue
            sample.append(str(path))
            sample_bytes += path.lstat().st_size
    before = upper_bytes()
    start = time.monotonic()
    subprocess.run(["setfacl", "-P", "-m", "u:motor-job:rX", "--", *sample], check=True)
    elapsed = time.monotonic() - start
    total = measurement[str(TOOL_ROOTS[0])]
    measurement["sample_setfacl"] = dict(
        files=len(sample), file_bytes=sample_bytes, elapsed_s=round(elapsed, 3),
        overlay_upper_growth_bytes=upper_bytes() - before,
        whole_tree_estimate_s=round(elapsed * total["file_bytes"] / max(sample_bytes, 1), 1))
    print("GUEST_TOOL_TREE_MEASUREMENT " + json.dumps(measurement, sort_keys=True), flush=True)
    if copied:
        raise RuntimeError(f"runner startup copied image tool trees into the overlay: {copied}")
    if closed:
        raise RuntimeError(f"image tool entries are not plainly world-readable: {closed}")
    print(TOOL_TREES, flush=True)


def root_path_probe(image=Path("/"), owner=0):
    # The host stages root_paths.py beside this file from its own checkout, so
    # a booted image that drifted from the build-time rule is caught here.
    spec = importlib.util.spec_from_file_location("root_paths", Path(__file__).with_name("root_paths.py"))
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load the staged root-owned path check")
    rule = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(rule)
    report = rule.check(rule.TENANT_ROOTS, image, owner)
    print("GUEST_ROOT_PATHS " + json.dumps(report, sort_keys=True), flush=True)
    if report["offender_count"]:
        raise RuntimeError(f"{report['offender_count']} path(s) root executes from are not root-owned "
                           f"without group/other write: {report['offenders']}")
    print(ROOT_PATHS, flush=True)


def runner_diagnostics():
    state: dict[str, Any] = {}
    for name, args in (
        ("unit", ["systemctl", "show", "motor-tenant-runner.service", "-p", "MainPID", "-p", "ActiveState", "-p", "SubState", "-p", "Result", "-p", "ExecMainStatus", "-p", "ExecStart"]),
        ("journal", ["journalctl", "-b", "-u", "motor-tenant-runner.service", "--no-pager", "-n", "80", "-o", "short-monotonic"]),
    ):
        try:
            result = subprocess.run(args, capture_output=True, text=True, timeout=2)
            state[name] = dict(code=result.returncode, stdout=result.stdout, stderr=result.stderr)
        except Exception as error:
            state[name] = dict(error=f"{type(error).__name__}: {error}")
    # The guest keeps no journal, so a runner that never binds its socket leaves
    # no log. Record what its threads and children are blocked in instead.
    state["processes"] = process_snapshot(state)
    path = Path("/tenant/state/main/runner.sock")
    try:
        info = path.lstat()
        socket_state: dict[str, Any] = dict(path=str(path), mode=oct(info.st_mode), uid=info.st_uid, inode=info.st_ino)
        state["socket"] = socket_state
        with socket.socket(socket.AF_UNIX) as peer:
            peer.settimeout(.25)
            peer.connect(str(path))
            pid, uid, gid = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, getattr(socket, "SO_PEERCRED"), 12))
            socket_state["peer"] = dict(pid=pid, uid=uid, gid=gid)
    except Exception as error:
        state["socket_error"] = f"{type(error).__name__}: {error}"
    print("GUEST_RUNNER_DIAGNOSTIC " + json.dumps(state), flush=True)


def socket_peer(inode):
    """The /proc/net rows that name a socket inode, so its protocol and address show."""
    rows = []
    for table in ("unix", "udp", "udp6", "tcp", "tcp6", "netlink", "raw", "raw6"):
        try:
            lines = Path(f"/proc/net/{table}").read_text().splitlines()
        except OSError:
            continue
        rows += [f"{table}: {line.strip()}" for line in lines[1:] if inode in line.split()]
    return rows or ["inode not found in /proc/net"]


def process_snapshot(state):
    """Each thread's wait channel for the runner and its descendants."""
    try:
        main = int(state["unit"]["stdout"].split("MainPID=", 1)[1].split()[0])
    except Exception as error:
        return dict(error=f"no MainPID: {type(error).__name__}: {error}")
    children: dict[int, list[int]] = {}
    try:
        entries = list(Path("/proc").iterdir())
    except OSError as error:
        return dict(error=f"no /proc: {type(error).__name__}: {error}")
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            parent = int((entry / "stat").read_text().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(parent, []).append(int(entry.name))
    found, pending = [], [main]
    while pending and len(found) < 64:
        pid = pending.pop()
        found.append(pid)
        pending.extend(children.get(pid, []))
    snapshot = []
    for pid in found:
        proc = Path(f"/proc/{pid}")
        try:
            cmdline = (proc / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")[:300]
            threads = []
            for task in sorted((proc / "task").iterdir(), key=lambda p: int(p.name))[:32]:
                stat = (task / "stat").read_text().rsplit(")", 1)[1].split()
                threads.append(dict(tid=int(task.name), comm=(task / "comm").read_text().strip(),
                                    state=stat[0], wchan=(task / "wchan").read_text().strip()))
            fds = {}
            for fd in sorted((proc / "fd").iterdir(), key=lambda p: int(p.name))[:32]:
                try:
                    fds[fd.name] = os.readlink(fd)
                except OSError as error:
                    fds[fd.name] = f"{type(error).__name__}"
            sockets = {}
            for name, target in fds.items():
                if target.startswith("socket:["):
                    sockets[name] = socket_peer(target[8:-1])
            try:
                stack = (proc / "stack").read_text()[-1200:]
            except OSError as error:
                stack = f"{type(error).__name__}: {error}"
            snapshot.append(dict(pid=pid, cmdline=cmdline, threads=threads, fds=fds,
                                 sockets=sockets, stack=stack))
        except OSError as error:
            snapshot.append(dict(pid=pid, error=f"{type(error).__name__}: {error}"))
    return snapshot


def diagnostic_window(stop):
    for _ in range(8):
        runner_diagnostics()
        if stop.wait(4):
            return


def run(*args, **kwargs):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, timeout=180, **kwargs)
    print(result.stdout, end="", flush=True)
    if result.returncode:
        raise RuntimeError(f"{args[0]} exited {result.returncode}")
    return result.stdout


def toolchain_probe(root):
    report = json.loads(run("python3", str(root / "toolchain-parity.py"),
                            "--check", "--role", "tenant"))
    (root / "parity-host.json").write_text(json.dumps(report, indent=2) + "\n")
    expected = (root / "gnutest.sha256").read_text().strip()
    if not report['passed'] or report["observed"]["tools"]["gnutest"]["sha256"] != expected:
        raise RuntimeError("booted package-qualified gnutest differs from the image inventory or parity failed")
    print(PARITY, flush=True)


def javascript_setup_probe(scratch):
    """The runner's own trusted JavaScript dependency setup, with the tool
    locations of its --tenant-vm role, run after attach: the tenant's /home and
    /Users are bound over the image's by now, so a setup tool the image left
    beneath them is missing here exactly as it would be for a job."""
    target = scratch / "javascript-setup"
    if target.exists():
        shutil.rmtree(target)
    output = run("/usr/local/bin/motor-runner", "check-javascript-setup", "--tenant-vm",
                 "--scratch", str(target))
    if RUNNER_JAVASCRIPT_PASS not in output.splitlines():
        raise RuntimeError("the runner's JavaScript setup check did not pass")
    shutil.rmtree(target)
    print(JAVASCRIPT_SETUP, flush=True)


def main():
    deadline = time.monotonic() + 120
    while subprocess.run(["systemctl", "is-active", "--quiet", "motor-tenant-runner.service"]).returncode:
        if time.monotonic() > deadline:
            raise RuntimeError("tenant runner did not become active after attachment")
        time.sleep(.1)
    run("/usr/local/libexec/hosted_image_check.py", "--tenant")
    name_service_probe()
    tool_tree_probe()
    root_path_probe()
    root = Path("/tenant/qualification")
    toolchain_probe(root)
    scratch = Path("/tenant/qualification-scratch")
    scratch.mkdir(exist_ok=True)
    javascript_setup_probe(scratch)
    env = dict(os.environ, TMPDIR=str(scratch), RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="")
    for key in list(env):
        if key.startswith("GIT_CONFIG_"):
            del env[key]
    output = run(str(root / "runner-tests"), TEST, "--ignored", "--exact", "--nocapture",
                 env=env, cwd=root)
    if f"test {TEST} ... ok" not in output or "1 passed; 0 failed; 0 ignored;" not in output:
        raise RuntimeError("the exact ignored isolation test did not run once successfully")
    print("PASS guest vsock isolation: 1 passed; 0 failed; 0 ignored", flush=True)
    # The final real job creates this file. Checking units afterward includes
    # failures from attachment, isolation and the post-restore job, not just boot.
    deadline = time.monotonic() + 600
    finish = Path("/Users/qualification/probe/qualification-finish")
    while not finish.is_file():
        if time.monotonic() > deadline:
            raise RuntimeError("post-restore completion marker did not arrive")
        time.sleep(.1)
    failed = run("systemctl", "--failed", "--no-legend", "--plain", "--no-pager").strip()
    if failed:
        raise RuntimeError(f"failed guest units: {failed}")
    print("PASS guest failed units: none", flush=True)


if __name__ == "__main__":
    stop = threading.Event()
    diagnostics = threading.Thread(target=diagnostic_window, args=(stop,), daemon=True)
    diagnostics.start()
    try:
        main()
    except Exception as error:
        print(f"FAIL guest qualification: {error}", flush=True)
        raise
    finally:
        stop.set()
        diagnostics.join(timeout=5)
