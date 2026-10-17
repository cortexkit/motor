#!/usr/bin/env python3
"""Operator-only direct-jailer experiment. No helper, hugepages, or production state."""

import argparse
import base64
from contextlib import closing
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import time
import uuid

GIB = 1024**3
RAM_MIB = 18432
HEADROOM_MIB = 512
REQUIRED_METRICS = (
    "cold_boot", "vsock", "base_snapshot_create", "base_restore_20g", "base_restore_24g",
    "network_override", "drive_substitute_20g", "drive_substitute_24g",
    "ficlone_20g", "ficlone_24g", "disk_attach_20g", "disk_attach_24g", "disk_detach",
    "suspend_snapshot_create", "suspend", "suspend_restore", "restore_memory_peak_bytes",
)


class Evidence:
    def __init__(self, output):
        self.output = output
        self.samples = {}
        self.raw = (output / "raw.jsonl").open("x", buffering=1)

    def record(self, kind, **fields):
        self.raw.write(json.dumps({"kind": kind, "at_ns": time.time_ns(), **fields}) + "\n")

    def run(self, *args, **kwargs):
        self.record("command", argv=list(map(str, args)))
        result = subprocess.run(list(map(str, args)), capture_output=True, text=True, **kwargs)
        self.record("command_result", returncode=result.returncode,
                    stdout=result.stdout, stderr=result.stderr)
        if result.returncode:
            raise RuntimeError(f"{shlex.join(list(map(str, args)))}: {result.stderr or result.stdout}")
        return result.stdout.strip()

    def sample(self, name, value, unit="ms", **detail):
        self.samples.setdefault(name, []).append(value)
        self.record("sample", name=name, value=value, unit=unit, **detail)
        print(f"{name}: {value:.3f} {unit}", flush=True)

    def measure(self, name, action):
        started = time.monotonic_ns()
        result = action()
        self.sample(name, (time.monotonic_ns() - started) / 1e6, result=result)
        return result


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.path))


def wait_for(action, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return action()
        except (OSError, RuntimeError, http.client.HTTPException, json.JSONDecodeError) as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(f"readiness timeout: {error}") from error
            time.sleep(0.02)


def recv_exact(sock, size):
    data = bytearray()
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            raise RuntimeError("unexpected EOF")
        data.extend(chunk)
    return bytes(data)


def send_frame(sock, frame):
    data = json.dumps(frame, separators=(",", ":")).encode()
    sock.sendall(struct.pack(">I", len(data)) + data)


def read_frame(sock):
    size, = struct.unpack(">I", recv_exact(sock, 4))
    if not 0 < size <= 1048576:
        raise RuntimeError(f"invalid frame size {size}")
    return json.loads(recv_exact(sock, size))


def hello(sock):
    send_frame(sock, {"type": "hello", "protocol_version": 1, "started": True})
    frame = read_frame(sock)
    if frame.get("type") != "hello" or frame.get("protocol_version") != 1:
        raise RuntimeError(f"runner hello failed: {frame}")
    return frame


class VM:
    def __init__(self, harness, disk, rootfs):
        self.h = harness
        self.e = harness.e
        self.identifier = "v" + uuid.uuid4().hex[:10]
        self.base = harness.work / self.identifier
        self.root = self.base / "firecracker" / self.identifier / "root"
        self.root.mkdir(parents=True)
        os.chown(self.root, harness.uid, harness.uid)
        # Resolve sockets through a directory fd so long checkout paths do not exceed sun_path.
        self.root_fd = os.open(self.root, getattr(os, "O_PATH") | os.O_DIRECTORY)
        self.socket_root = Path(f"/proc/self/fd/{self.root_fd}")
        self.mounts = []
        self.proc = None
        self.cgroup = Path("/sys/fs/cgroup") / harness.group / self.identifier
        self.log = (self.e.output / f"{self.identifier}-console.log").open("x")
        try:
            self.bind(harness.kernel, "kernel", readonly=True)
            self.bind(rootfs, "rootfs.ext4")
            self.bind(disk, "tenant.img")
            self.bind(harness.state, "state", directory=True)
            args = [harness.jailer, "--id", self.identifier, "--exec-file", harness.firecracker,
                    "--uid", str(harness.uid), "--gid", str(harness.uid),
                    "--chroot-base-dir", str(self.base), "--netns", f"/run/netns/{harness.netns}",
                    "--cgroup-version", "2", "--parent-cgroup", harness.group,
                    "--cgroup", "cpu.max=100000 100000",
                    "--cgroup", f"memory.max={(RAM_MIB + HEADROOM_MIB) * 1024**2}",
                    "--cgroup", "pids.max=66", "--", "--api-sock", "/api.sock"]
            self.e.record("jailer_launch", argv=args)
            self.proc = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)
            wait_for(lambda: self.api("GET", "/version"), timeout=10)
            actual = Path(f"/proc/{self.proc.pid}/cgroup").read_text().strip()
            if actual != f"0::/{harness.group}/{self.identifier}":
                raise RuntimeError(f"unexpected jailer cgroup: {actual}")
            self.e.record("cgroup", path=str(self.cgroup),
                          memory_max=(self.cgroup / "memory.max").read_text().strip(),
                          cpu_max=(self.cgroup / "cpu.max").read_text().strip())
            if int((self.cgroup / "memory.max").read_text()) != (RAM_MIB + HEADROOM_MIB) * 1024**2:
                raise RuntimeError("jailer did not apply the requested memory.max")
        except BaseException:
            self.close()
            raise

    def bind(self, source, name, readonly=False, directory=False):
        target = self.root / name
        if directory:
            target.mkdir()
        else:
            target.touch()
        self.e.run("mount", "--bind", source, target)
        self.mounts.append(target)
        if readonly:
            self.e.run("mount", "-o", "remount,bind,ro", target)

    def api(self, method, path, body=None):
        self.e.record("api_request", method=method, path=path, body=body)
        with closing(UnixHTTP(self.socket_root / "api.sock")) as client:
            client.request(method, path, None if body is None else json.dumps(body),
                           {"Content-Type": "application/json"})
            response = client.getresponse()
            payload = response.read().decode()
            self.e.record("api_response", status=response.status, body=payload)
            if response.status not in (200, 204):
                raise RuntimeError(f"{method} {path}: {response.status} {payload}")
            return {"status": response.status, "body": payload}

    def boot(self):
        self.api("PUT", "/machine-config", {"vcpu_count": 2, "mem_size_mib": RAM_MIB,
                                            "smt": False, "track_dirty_pages": False})
        self.api("PUT", "/boot-source", {"kernel_image_path": "/kernel", "boot_args":
                 "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/sbin/init "
                 "systemd.unified_cgroup_hierarchy=1 random.trust_cpu=on net.ifnames=0"})
        for drive_id, filename, root in [("rootfs", "/rootfs.ext4", True), ("tenant", "/tenant.img", False)]:
            self.api("PUT", f"/drives/{drive_id}", {"drive_id": drive_id, "path_on_host": filename,
                     "is_root_device": root, "is_read_only": False, "cache_type": "Writeback"})
        self.api("PUT", "/network-interfaces/eth0", {"iface_id": "eth0", "host_dev_name": "tap-a",
                                                   "guest_mac": "06:00:ac:10:00:02"})
        self.api("PUT", "/vsock", {"guest_cid": 3, "uds_path": "/vsock.sock"})
        self.api("PUT", "/actions", {"action_type": "InstanceStart"})
        return self.ready()

    def connect(self, port):
        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.settimeout(120)
        try:
            conn.connect(str(self.socket_root / "vsock.sock"))
            conn.sendall(f"CONNECT {port}\n".encode())
            reply = bytearray()
            while not reply.endswith(b"\n") and len(reply) < 100:
                reply.extend(recv_exact(conn, 1))
            if not reply.startswith(b"OK "):
                raise RuntimeError(f"vsock CONNECT failed: {reply!r}")
            return conn
        except BaseException:
            conn.close()
            raise

    def control(self, op, **args):
        with self.connect(54) as conn:
            conn.sendall(json.dumps({"op": op, **args}).encode() + b"\n")
            reply = json.loads(conn.makefile("rb").readline(65536))
        self.e.record("guest_control", op=op, args=args, reply=reply)
        if not reply["ok"]:
            raise RuntimeError(f"guest {op}: {reply['error']}")
        return reply["result"]

    def ready(self, runner=False):
        result = wait_for(lambda: self.control("ready"))
        if runner:
            with self.connect(52) as conn:
                result["runner_hello"] = hello(conn)
        return result

    def snapshot(self, name):
        result = self.api("PUT", "/snapshot/create", {"snapshot_type": "Full",
                          "snapshot_path": f"/state/{name}.vmstate", "mem_file_path": f"/state/{name}.mem"})
        for path in [self.h.state / f"{name}.vmstate", self.h.state / f"{name}.mem", self.h.state]:
            fd = os.open(path, os.O_RDONLY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        return result

    def load(self, name, tap):
        return self.api("PUT", "/snapshot/load", {"snapshot_path": f"/state/{name}.vmstate",
                        "mem_backend": {"backend_type": "File", "backend_path": f"/state/{name}.mem"},
                        "enable_diff_snapshots": False, "resume_vm": False,
                        "network_overrides": [{"iface_id": "eth0", "host_dev_name": tap}]})

    def pause(self):
        self.api("PATCH", "/vm", {"state": "Paused"})

    def resume(self):
        self.api("PATCH", "/vm", {"state": "Resumed"})

    def peak(self):
        value = int((self.cgroup / "memory.peak").read_text())
        self.e.record("memory", memory_peak=value,
                      memory_events=(self.cgroup / "memory.events").read_text())
        return value

    def close(self):
        # Stop the VMM before removing disk bind mounts; deleting a pathname does not close its fd.
        failures = []
        if self.proc is not None:
            try:
                if self.proc.poll() is None:
                    if (self.cgroup / "cgroup.kill").exists():
                        (self.cgroup / "cgroup.kill").write_text("1")
                    else:
                        self.proc.kill()
                    self.proc.wait(timeout=10)
                self.proc = None
            except Exception as error:
                failures.append(str(error))
        if self.proc is None:
            for target in reversed(self.mounts[:]):
                try:
                    self.e.run("umount", target)
                    self.mounts.remove(target)
                except Exception as error:
                    failures.append(str(error))
        self.log.close()
        if self.root_fd is not None:
            os.close(self.root_fd)
            self.root_fd = None
        if self.cgroup.exists():
            try:
                self.cgroup.rmdir()
            except OSError as error:
                failures.append(str(error))
        if not self.mounts and self.proc is None and self.base.exists():
            shutil.rmtree(self.base)
        if failures:
            raise RuntimeError("; ".join(failures))


class Harness:
    def __init__(self, args, evidence):
        self.e = evidence
        self.work = args.work.resolve()
        self.kernel = args.kernel.resolve()
        self.rootfs = args.rootfs.resolve()
        self.firecracker = str(Path(os.environ["FIRECRACKER"]).resolve())
        self.jailer = str(Path(os.environ["JAILER"]).resolve())
        self.uid = args.uid
        token = uuid.uuid4().hex[:8]
        self.netns = "msp-" + token
        self.group = "motor-spike-" + token
        self.state = self.work / "snapshots"
        self.active = None
        self.netns_created = False

    def prepare(self):
        self.work.mkdir()
        self.state.mkdir()
        os.chown(self.state, self.uid, self.uid)
        self.e.run("ip", "netns", "add", self.netns)
        self.netns_created = True
        self.e.run("ip", "-n", self.netns, "link", "set", "lo", "up")
        for name, address in [("tap-a", "192.0.2.1/30"), ("tap-b", "192.0.2.5/30")]:
            self.e.run("ip", "-n", self.netns, "tuntap", "add", name, "mode", "tap", "user", str(self.uid))
            self.e.run("ip", "-n", self.netns, "addr", "add", address, "dev", name)
            self.e.run("ip", "-n", self.netns, "link", "set", name, "up")
        # Linux's weak-host ARP behavior must not make the wrong-tap test accidentally succeed.
        self.e.run("ip", "netns", "exec", self.netns, "sysctl", "-w", "net.ipv4.conf.all.arp_ignore=1")
        for gib in (1, 20, 24):
            path = self.disk(gib)
            self.e.run("fallocate", "-l", str(gib * GIB), path)
            self.e.run("mkfs.xfs", "-f", "-m", "reflink=1", path)
            os.chown(path, self.uid, self.uid)
            if path.stat().st_blocks * 512 < path.stat().st_size:
                raise RuntimeError(f"disk is sparse: {path}")

    def disk(self, gib):
        return self.work / f"tenant-{gib}g.xfs"

    def copy_root(self, source, name="run.ext4"):
        target = self.work / name
        if target.exists():
            target.unlink()
        self.e.run("cp", "--reflink=always", source, target)
        os.chown(target, self.uid, self.uid)
        return target

    def vm(self, disk, root):
        if self.active is not None:
            raise RuntimeError("refusing to attach a disk to two VMs")
        self.active = VM(self, disk, root)
        return self.active

    def stop(self):
        if self.active:
            self.active.close()
            self.active = None

    def tap_rx(self, tap):
        links = json.loads(self.e.run("ip", "-n", self.netns, "-j", "-s", "link", "show", tap))
        return links[0]["stats64"]["rx"]["packets"]

    def check_network_override(self, vm):
        before = self.tap_rx("tap-b")
        result = vm.control("network", profile="b")
        after = self.tap_rx("tap-b")
        if after <= before:
            raise RuntimeError("network override did not send packets through tap-b")
        return {**result, "tap_b_rx_before": before, "tap_b_rx_after": after}

    def assert_detached(self, disk):
        expected = disk.stat()
        holders = []
        for process in Path("/proc").iterdir():
            if not process.name.isdigit():
                continue
            try:
                for fd in (process / "fd").iterdir():
                    try:
                        info = fd.stat()
                        if (info.st_dev, info.st_ino) == (expected.st_dev, expected.st_ino):
                            holders.append(str(fd))
                    except FileNotFoundError:
                        pass
            except FileNotFoundError:
                pass
        if holders:
            raise RuntimeError(f"tenant disk still open: {holders}")
        return {"open_fds": holders, "disk_bytes": expected.st_size, "disk_preserved": disk.exists()}

    def close(self):
        failures = []
        try:
            self.stop()
        except Exception as error:
            failures.append(str(error))
        if self.netns_created:
            try:
                self.e.run("ip", "netns", "delete", self.netns)
                self.netns_created = False
            except Exception as error:
                failures.append(str(error))
        parent = Path("/sys/fs/cgroup") / self.group
        if parent.exists():
            try:
                parent.rmdir()
            except OSError as error:
                failures.append(str(error))
        if failures:
            raise RuntimeError("; ".join(failures))


def isolated_job(harness, vm):
    fixture = harness.work / ("fixture-" + uuid.uuid4().hex[:8])
    fixture.mkdir()
    run = harness.e.run
    run("git", "init", "--quiet", fixture)
    (fixture / "input").write_text("spike isolated job\n")
    run("git", "-C", fixture, "add", "input")
    run("git", "-C", fixture, "-c", "user.name=Spike", "-c", "user.email=spike@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture")
    commit = run("git", "-C", fixture, "rev-parse", "HEAD")
    tree = run("git", "-C", fixture, "rev-parse", "HEAD^{tree}")
    run("git", "-C", fixture, "bundle", "create", fixture / "input.bundle", "HEAD")
    bundle = (fixture / "input.bundle").read_bytes()
    request = {"type": "run", "job_id": str(uuid.uuid4()), "workspace_key": "/workspace/probe",
               "repository_root": "/workspace/probe", "cwd": "/workspace/probe",
               "command": "test \"$(id -un)\" = motor-job && "
                          "test ! -e /var/lib/motor-spike/host-only && "
                          "python3 -c 'import socket; assert socket.if_nameindex() == [(1, \"lo\")]' && "
                          "echo SPIKE_ISOLATED_JOB_PASS",
               "env": {}, "weight_hint": 1, "timeout": 30, "queue_wait_limit_s": 30,
               "snapshot_commit": commit, "tree_hash": tree, "integration_tip": commit,
               "siblings": [], "bundles": [{"index": 0, "repository": "/workspace/probe",
               "length": len(bundle), "sha256": hashlib.sha256(bundle).hexdigest()}]}
    output = bytearray()
    with vm.connect(52) as conn:
        hello(conn)
        send_frame(conn, request)
        frame = read_frame(conn)
        if frame.get("type") != "accepted":
            raise RuntimeError(f"isolated job not accepted: {frame}")
        send_frame(conn, {"type": "bundle_chunk", "index": 0, "offset": 0,
                          "data": base64.b64encode(bundle).decode()})
        while True:
            frame = read_frame(conn)
            harness.e.record("job_frame", frame=frame)
            if frame["type"] == "output":
                output.extend(base64.b64decode(frame["data"], validate=True))
            if frame["type"] == "terminal":
                if frame["outcome"] != {"type": "exit", "code": 0} or b"SPIKE_ISOLATED_JOB_PASS" not in output:
                    raise RuntimeError(f"isolated job failed: {frame}; {output!r}")
                return {"terminal": frame, "output": output.decode()}


def snapshot_space(harness):
    free = shutil.disk_usage(harness.work).free
    if free < (128 + 16) * GIB + RAM_MIB * 1024**2:
        raise RuntimeError("snapshot would encroach on the 128 GiB fleet floor plus 16 GiB reserve")


def experiment(harness):
    e = harness.e
    for iteration in range(1, 6):
        e.record("iteration", number=iteration)
        for path in harness.state.iterdir():
            path.unlink()
        root = harness.copy_root(harness.rootfs)

        def cold_boot():
            vm = harness.vm(harness.disk(1), root)
            return vm.boot()

        e.measure("cold_boot", cold_boot)
        vm = harness.active
        e.measure("vsock", lambda: vm.control("ready"))
        vm.control("network", profile="a")
        vm.control("sync")
        vm.pause()
        snapshot_space(harness)
        e.measure("base_snapshot_create", lambda: vm.snapshot("base"))
        harness.stop()
        base_root = harness.copy_root(root, "base.ext4")

        for gib in (20, 24):
            root = harness.copy_root(base_root)
            started = time.monotonic_ns()
            vm = harness.vm(harness.disk(1), root)
            vm.load("base", "tap-b")
            # A snapshot remembers the placeholder's capacity. Updating only the bind mount
            # cannot notify virtio-blk of a different-sized tenant disk.
            def substitute():
                vm.bind(harness.disk(gib), "replacement.img")
                return vm.api("PATCH", "/drives/tenant",
                              {"drive_id": "tenant", "path_on_host": "/replacement.img"})

            e.measure(f"drive_substitute_{gib}g", substitute)
            vm.resume()
            vm.ready()
            if gib == 20:
                e.measure("network_override", lambda: harness.check_network_override(vm))
            else:
                harness.check_network_override(vm)
            e.measure(f"disk_attach_{gib}g", lambda: vm.control("attach", expected_bytes=gib * GIB))
            vm.ready(runner=True)
            e.sample(f"base_restore_{gib}g", (time.monotonic_ns() - started) / 1e6)
            e.measure(f"ficlone_{gib}g", lambda: vm.control("ficlone"))
            isolated_job(harness, vm)
            if gib == 20:
                cache = vm.control("cache_fill")
                e.record("suspend_workload", disk_bytes=20 * GIB, **cache)
                snapshot_space(harness)

                def suspend():
                    vm.control("sync")
                    vm.pause()
                    e.measure("suspend_snapshot_create", lambda: vm.snapshot("suspend"))
                    harness.stop()
                    return harness.assert_detached(harness.disk(20))

                e.measure("suspend", suspend)
                # Restored kernel filesystem caches require the backing images from the same pause.
                started = time.monotonic_ns()
                vm = harness.vm(harness.disk(20), root)
                vm.bind(harness.disk(20), "replacement.img")
                vm.load("suspend", "tap-b")
                vm.resume()
                result = vm.ready(runner=True)
                e.sample("suspend_restore", (time.monotonic_ns() - started) / 1e6, result=result)
                e.sample("restore_memory_peak_bytes", vm.peak(), unit="bytes")
                e.record("cache_restored", result=vm.control("cache_verify"),
                         memory_peak_after_cache_read_bytes=vm.peak())
                isolated_job(harness, vm)

                def detach():
                    harness.stop()
                    return harness.assert_detached(harness.disk(20))

                e.measure("disk_detach", detach)
            else:
                harness.stop()
                harness.assert_detached(harness.disk(24))


def summarize(samples, error=None):
    metrics = {name: {"n": len(values), "p50": statistics.median(values), "samples": values,
                      "unit": "bytes" if name.endswith("_bytes") else "ms"}
               for name, values in samples.items()}
    missing = [name for name in REQUIRED_METRICS if len(samples.get(name, [])) != 5]
    reasons = ([error] if error else []) + ([f"not five runs: {', '.join(missing)}"] if missing else [])
    if samples.get("suspend_restore") and statistics.median(samples["suspend_restore"]) > 5000:
        reasons.append("suspend restore p50 exceeds 5 seconds")
    if samples.get("restore_memory_peak_bytes") and max(samples["restore_memory_peak_bytes"]) > (RAM_MIB + HEADROOM_MIB) * 1024**2:
        reasons.append("restore needs more than 512 MiB of memory headroom")
    return {"metrics": metrics, "verdict": "replan" if reasons else "go", "reasons": reasons}


def preflight(args):
    if os.geteuid() != 0 or platform.system() != "Linux" or platform.machine() != "x86_64":
        raise RuntimeError("need an operator running as root on a dedicated Linux x86_64 KVM host")
    for tool in ["ip", "sysctl", "mount", "umount", "fallocate", "mkfs.xfs", "cp", "git", "findmnt", "sha256sum"]:
        if not shutil.which(tool):
            raise RuntimeError(f"missing prerequisite: {tool}")
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise RuntimeError("missing readable/writable /dev/kvm")
    if not Path("/dev/net/tun").exists():
        raise RuntimeError("missing /dev/net/tun (load tun)")
    for variable in ["FIRECRACKER", "JAILER"]:
        path = Path(os.environ.get(variable, "/nonexistent"))
        if not path.is_absolute() or not os.access(path, os.X_OK):
            raise RuntimeError(f"set {variable} to an absolute executable path")
        version = subprocess.check_output([path, "--version"], text=True)
        if "1.17.0" not in [token.lstrip("v") for token in version.split()]:
            raise RuntimeError(f"{variable} must be the official 1.17.0 tool: {version}")
    if Path(os.environ["FIRECRACKER"]).name != "firecracker":
        raise RuntimeError("FIRECRACKER basename must be firecracker for the jailer chroot layout")
    for path in [args.kernel, args.rootfs]:
        if not path.is_file():
            raise RuntimeError(f"missing image input: {path}")
    for path in [args.work, args.results]:
        if path.exists() or not path.parent.is_dir():
            raise RuntimeError(f"need a new directory with an existing parent: {path}")
    if shutil.disk_usage(args.work.parent).free < (128 + 16 + 128) * GIB:
        raise RuntimeError("need >=272 GiB free: 128 GiB fleet floor + 16 GiB reserve + 128 GiB peak budget")
    available = next(int(line.split()[1]) * 1024 for line in Path("/proc/meminfo").read_text().splitlines()
                     if line.startswith("MemAvailable:"))
    if available < 22 * GIB:
        raise RuntimeError("need at least 22 GiB MemAvailable for the 18 GiB VM")
    controllers = Path("/sys/fs/cgroup/cgroup.controllers")
    if not controllers.exists() or not {"cpu", "memory", "pids"} <= set(controllers.read_text().split()):
        raise RuntimeError("need unified cgroup v2 with cpu, memory and pids controllers")
    import pwd
    try:
        pwd.getpwuid(args.uid)
    except KeyError:
        pass
    else:
        raise RuntimeError(f"jailer uid {args.uid} has an account; choose an unused --uid")
    filesystem = subprocess.check_output(["findmnt", "-n", "-o", "FSTYPE", "-T", args.work.parent], text=True).strip()
    if filesystem != "xfs":
        raise RuntimeError(f"work directory must be on reflink-capable XFS, not {filesystem}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True, help="new short scratch path on XFS")
    parser.add_argument("--results", type=Path, required=True, help="new persistent raw evidence directory")
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--rootfs", type=Path, required=True)
    parser.add_argument("--uid", type=int, default=290000)
    args = parser.parse_args()
    preflight(args)
    args.results.mkdir(mode=0o700)
    evidence = Evidence(args.results)
    harness = Harness(args, evidence)
    error = None

    def interrupted(signum, _frame):
        raise RuntimeError(f"interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        evidence.record("invocation", argv=sys.argv, kernel=platform.release(), python=platform.python_version(),
                        guest_ram_mib=RAM_MIB, memory_headroom_mib=HEADROOM_MIB)
        for path in [args.kernel, args.rootfs, Path(harness.firecracker), Path(harness.jailer)]:
            evidence.record("input", path=str(path), sha256=evidence.run("sha256sum", path))
        evidence.run(harness.firecracker, "--version")
        evidence.run(harness.jailer, "--version")
        harness.prepare()
        experiment(harness)
    except Exception as failure:
        error = str(failure)
        evidence.record("failure", error=error)
        print(f"SPIKE FAILED: {error}", file=sys.stderr, flush=True)
    finally:
        try:
            harness.close()
        except Exception as failure:
            error = f"{error or ''}; CLEANUP FAILED: {failure}; operator must inspect {harness.work}"
            evidence.record("cleanup_failure", error=error)
    summary = summarize(evidence.samples, error)
    summary.update({"command": shlex.join(sys.argv), "runs": 5, "guest_ram_mib": RAM_MIB,
                    "headroom_mib": HEADROOM_MIB, "work": str(harness.work),
                    "results": str(args.results.resolve())})
    (args.results / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    evidence.raw.close()
    print(json.dumps(summary, indent=2), flush=True)
    print(f"{summary['verdict']}: " + ("; ".join(summary["reasons"]) or
          "five-run vsock, network override, drive substitution and FICLONE passed; suspend restore p50 <=5 s"))
    return 0 if summary["verdict"] == "go" else 1


if __name__ == "__main__":
    sys.exit(main())
