#!/usr/bin/env python3
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import socket
import struct
import tempfile
import threading
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("provision", Path(__file__).with_name("provision.py"))
assert spec is not None and spec.loader is not None
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
# Kept before setUp replaces it, for the one test that reads a fake /proc.
REAL_RUNNER_PROCESSES = p.runner_processes


def config():
    return p.checked(dict(tenant="a", sub="acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", vm="vm-a", image_version="v1", vcpu_ceiling=2,
                          guest_ram_mib=18432, disk_quota_bytes=21474836480, cpuset="0-1", tap_index=1, idle_suspend_s=60,
                          host_floor_bytes=2147483648, resolvers=["1.1.1.1"]))


class ProvisionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        # runner_processes would read this machine's real /proc; tests that need
        # a runner pass one explicitly.
        self.patch = mock.patch.multiple(p, ROOT=self.root, REGISTRY=self.root / "registry", PROVISION=self.root / "provision",
                                         BINDINGS=self.root / "host/tenants.json",
                                         runner_processes=lambda: [])
        self.patch.start()
        for directory in [p.REGISTRY, p.PROVISION, p.BINDINGS.parent, p.ROOT / "tenants"]:
            directory.mkdir(parents=True)
        self.image = self.root / "tenant-images/v1"
        self.image.mkdir(parents=True)
        (self.image / "vmlinux").write_bytes(b"kernel")
        (self.image / "rootfs").write_bytes(b"root")
        (self.image / "manifest.json").write_text(json.dumps(dict(version="v1", kernel_sha256=hashlib.sha256(b"kernel").hexdigest(), rootfs_sha256=hashlib.sha256(b"root").hexdigest())))

    def switch_records(self):
        c = config()
        provision_path = p.PROVISION / "a.json"
        provision_path.write_text(json.dumps(c, indent=2) + "\n")
        record = {key: value for key, value in c.items() if key not in {"tenant", "sub", "vm", "resolvers"}}
        record.update(disk=str(p.ROOT / "tenants/a/disk.img"), generation=str(p.ROOT / "tenants/a/generation"),
                      suspend=str(p.ROOT / "tenants/a/suspend"))
        registry_path = p.REGISTRY / "a.json"
        registry_path.write_text(json.dumps(record, indent=2) + "\n")
        suspend = p.ROOT / "tenants/a/suspend"
        suspend.mkdir(parents=True)
        (self.root / "tenant-images/v2").mkdir(parents=True)
        (self.root / "tenant-images/v2/vmlinux").write_bytes(b"kernel-v2")
        (self.root / "tenant-images/v2/rootfs").write_bytes(b"root-v2")
        manifest = dict(version="v2", kernel_sha256=hashlib.sha256(b"kernel-v2").hexdigest(),
                        rootfs_sha256=hashlib.sha256(b"root-v2").hexdigest())
        (self.root / "tenant-images/v2/manifest.json").write_text(json.dumps(manifest))
        return c, provision_path, registry_path, suspend, manifest

    @staticmethod
    def fake_base(_config, _image, base):
        base.mkdir(parents=True)
        for name in ("base.vmstate", "base.memory"):
            (base / name).write_bytes(b"base")

    def tearDown(self):
        self.patch.stop()
        self.tmp.cleanup()

    def test_switch_image_refuses_when_either_service_is_running(self):
        _c, provision_path, registry_path, _suspend, _manifest = self.switch_records()
        old_config = provision_path.read_bytes()
        old_record = registry_path.read_bytes()
        for active in ("motor-host.service", "motor-vm-helper.service"):
            with self.subTest(active=active):
                states = {"motor-host.service": "inactive", "motor-vm-helper.service": "inactive"}
                states[active] = "active"
                make_base = mock.Mock(side_effect=self.fake_base)
                validate = mock.Mock()
                with self.assertRaisesRegex(ValueError, "must be stopped"):
                    p.switch_image("a", "v2", make_base=make_base, check_service=states.get,
                                   validate_image=validate, output=lambda _line: None)
                validate.assert_not_called()
                make_base.assert_not_called()
                self.assertEqual(provision_path.read_bytes(), old_config)
                self.assertEqual(registry_path.read_bytes(), old_record)

    def test_switch_image_runs_the_hosted_image_inventory_command(self):
        with mock.patch.object(p.subprocess, "run") as execute:
            p.inventory_image(self.image)
        command = execute.call_args.args[0]
        self.assertEqual(command[1:], [str(Path(__file__).resolve().parents[1] / "tenant-image" / "build.py"),
                                       "inventory", str(self.image)])
        self.assertTrue(execute.call_args.kwargs["check"])

    def test_switch_image_updates_both_records_and_builds_the_size_base(self):
        _c, provision_path, registry_path, _suspend, _manifest = self.switch_records()
        make_base = mock.Mock(side_effect=self.fake_base)
        validate = mock.Mock()
        messages = []
        p.switch_image("a", "v2", make_base=make_base, check_service=lambda _unit: "inactive",
                       validate_image=validate, output=messages.append)
        self.assertEqual(json.loads(provision_path.read_text())["image_version"], "v2")
        self.assertEqual(json.loads(registry_path.read_text())["image_version"], "v2")
        self.assertEqual(make_base.call_args.args[1:], (self.root / "tenant-images/v2", self.root / "tenant-images/v2/base/2-18432"))
        validate.assert_called_once_with(self.root / "tenant-images/v2")
        self.assertIn("a: v1 -> v2", messages)
        self.assertTrue(any("switch-image a v1" in line for line in messages))

    def test_switch_image_moves_a_mismatched_suspend_snapshot_and_prints_its_path(self):
        _c, provision_path, registry_path, suspend, _manifest = self.switch_records()
        old_manifest = json.loads((self.image / "manifest.json").read_text())
        (suspend / "key.json").write_text(json.dumps({**old_manifest, "generation": 0}))
        (suspend / "suspend.vmstate").write_bytes(b"old-state")
        (suspend / "suspend.memory").write_bytes(b"old-memory")
        make_base = mock.Mock(side_effect=self.fake_base)
        messages = []
        moved = p.switch_image("a", "v2", make_base=make_base, check_service=lambda _unit: "inactive",
                               validate_image=lambda _image: None, output=messages.append)
        self.assertIsNotNone(moved)
        self.assertFalse((suspend / "suspend.vmstate").exists())
        self.assertEqual((moved / "suspend.vmstate").read_bytes(), b"old-state")
        self.assertEqual((moved / "suspend.memory").read_bytes(), b"old-memory")
        self.assertTrue(any(str(moved) in line for line in messages))
        self.assertEqual(json.loads(provision_path.read_text())["image_version"], "v2")
        self.assertEqual(json.loads(registry_path.read_text())["image_version"], "v2")

    def test_switch_image_is_idempotent_and_can_roll_back_to_the_old_image(self):
        _c, provision_path, registry_path, _suspend, _manifest = self.switch_records()
        make_base = mock.Mock(side_effect=self.fake_base)
        validate = mock.Mock()
        messages = []
        with mock.patch.object(p, "atomic_pair", wraps=p.atomic_pair) as persist:
            for version in ("v2", "v2", "v1"):
                p.switch_image("a", version, make_base=make_base, check_service=lambda _unit: "inactive",
                               validate_image=validate, output=messages.append)
        self.assertEqual(persist.call_count, 2)
        self.assertEqual(json.loads(provision_path.read_text())["image_version"], "v1")
        self.assertEqual(json.loads(registry_path.read_text())["image_version"], "v1")
        self.assertEqual(make_base.call_count, 2)
        self.assertIn("a: v2 -> v1", messages)
        self.assertTrue(any("switch-image a v2" in line for line in messages))

    def test_switch_image_rolls_back_the_first_record_if_the_second_replace_fails(self):
        _c, provision_path, registry_path, suspend, _target_manifest = self.switch_records()
        old_manifest = json.loads((self.image / "manifest.json").read_text())
        (suspend / "key.json").write_text(json.dumps(old_manifest))
        (suspend / "suspend.vmstate").write_bytes(b"old-state")
        old_config = provision_path.read_bytes()
        old_record = registry_path.read_bytes()
        real_replace = p.os.replace
        failed = False

        def replace_once(source, destination):
            nonlocal failed
            if Path(destination) == provision_path and not failed:
                failed = True
                raise OSError("injected second-record failure")
            return real_replace(source, destination)

        with mock.patch.object(p.os, "replace", side_effect=replace_once), self.assertRaisesRegex(OSError, "second-record"):
            p.switch_image("a", "v2", make_base=self.fake_base, check_service=lambda _unit: "inactive",
                           validate_image=lambda _image: None, output=lambda _line: None)
        self.assertEqual(provision_path.read_bytes(), old_config)
        self.assertEqual(registry_path.read_bytes(), old_record)
        self.assertEqual((suspend / "suspend.vmstate").read_bytes(), b"old-state")
        self.assertEqual(len(list(suspend.parent.glob("suspend.stale-*"))), 0)
        self.assertEqual(list(p.PROVISION.glob("*.pending")), [])

    def test_invalid_names_and_unknown_options(self):
        for name in ["", ".", "..", "foo/bar", "a\0b"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                p.component(name)
        bad = config()
        bad["path"] = "/outside"
        with self.assertRaises(ValueError):
            p.checked(bad)

    def test_runner_args_small_disk_uses_bytes_and_quarter_cache(self):
        args = p.runner_args(config())
        for option in ["--max-weight 2", "--rustc-slots 2", "--sccache-size-gib 5", "--free-space-floor-bytes 2147483648", "--owned-workspace-root /Users", "--owned-workspace-root /home"]:
            self.assertIn(option, args)

    def test_new_base_space_is_checked_before_any_mutation(self):
        c = config()
        needed = c["disk_quota_bytes"] + 2 * c["guest_ram_mib"] * 1048576 + c["host_floor_bytes"] + 512 * 1048576
        make_base = mock.Mock()
        execute = mock.Mock()
        before = sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*"))
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": needed - 1})), self.assertRaisesRegex(ValueError, "free bytes"):
            p.provision(c, make_base, execute)
        make_base.assert_not_called()
        execute.assert_not_called()
        self.assertEqual(before, sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*")))

    def test_existing_base_space_still_reserves_suspend_room(self):
        c = config()
        (self.image / "base/2-18432").mkdir(parents=True)
        for name in ("base.vmstate", "base.memory"):
            (self.image / "base/2-18432" / name).touch()
        needed = c["disk_quota_bytes"] + c["guest_ram_mib"] * 1048576 + c["host_floor_bytes"]
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": needed - 1})), self.assertRaisesRegex(ValueError, "free bytes"):
            p.provision(c)
        self.assertFalse((self.root / "tenants/a").exists())

    def test_provision_orders_allocation_format_generation_registry_and_binding(self):
        c = config()
        events = []
        records = {}
        def execute(*args):
            events.append(args[0])
            if args[0] == "fallocate":
                self.assertEqual(args[1:3], ("-l", 21474836480))
                args[-1].write_bytes(b"allocated")
            if args[0] == "mkfs.xfs":
                self.assertEqual(args[1:3], ("-m", "reflink=1"))
        original = p.atomic
        def atomic(path, value):
            events.append(path.name)
            records[path.name] = value
            original(path, value)
        def base(c, image, path):
            events.append("base")
            path.mkdir(parents=True)
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": 2**63})), mock.patch.object(p, "atomic", side_effect=atomic):
            p.provision(c, base, execute)
        self.assertEqual(events, ["base", "fallocate", "mkfs.xfs", "mount", "runner-args", "umount", "generation", "a.json", "tenants.json"])
        record = json.loads((p.REGISTRY / "a.json").read_text())
        self.assertEqual(record["disk"], str(self.root / "tenants/a/disk.img"))
        self.assertEqual(record["image_version"], "v1")
        self.assertEqual(json.loads(p.BINDINGS.read_text()), {"vm-a": {"tenant": "a", "sub": c["sub"]}})
        self.assertEqual(records["runner-args"], p.runner_args(c))
        self.assertEqual((self.root / "tenants/a/generation").read_text(), "0\n")

    def test_runner_refusal_floor_comes_from_the_live_runner_arguments(self):
        home = {"HOME": "/home/ubuntu"}
        gib = 1024**3
        cases = [
            (["/home/ubuntu/.cargo/bin/motor-runner", "serve", "--instance", "main", "--max-weight", "80", "--tenant-vm"], 128 * gib),
            (["motor-runner", "serve", "--free-space-floor-bytes", str(64 * gib)], 64 * gib),
            # The eviction threshold is not the floor: below it nothing fails.
            (["motor-runner-windows", "serve", "--free-space-floor-bytes", str(64 * gib), "--free-space-eviction-bytes", str(100 * gib)], 64 * gib),
            (["motor-runner", "serve", "--free-space-floor-bytes", "0"], 0),
        ]
        for argv, floor in cases:
            with self.subTest(argv=argv):
                self.assertEqual(p.runner_floor(argv, home), (Path("/home/ubuntu/.local/share/cortexkit/motor-runner"), floor))
        self.assertEqual(p.runner_floor(["motor-runner", "serve", "--state-root", "/srv/build/state"], None)[0], Path("/srv/build/state"))
        self.assertEqual(p.runner_floor(["motor-runner", "serve"], {"XDG_DATA_HOME": "/data", "HOME": "/h"})[0], Path("/data/cortexkit/motor-runner"))
        for argv, environ in ((["motor-runner", "serve"], None), (["motor-runner", "serve", "--max-weight"], home)):
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                p.runner_floor(argv, environ)

    def test_runner_processes_finds_serving_runners_only(self):
        proc = self.root / "proc"
        for pid, argv in ((10, ["/home/ubuntu/.cargo/bin/motor-runner", "serve", "--instance", "main"]),
                          (11, ["/home/ubuntu/.cargo/bin/motor-runner-windows", "serve", "--instance", "windows"]),
                          (12, ["/home/ubuntu/.cargo/bin/motor-runner", "drain", "--instance", "main"]),
                          (13, ["/usr/local/libexec/motor-runner-permissions", "physical-chmod"]),
                          (14, ["motor-runner"]), (15, ["bash"])):
            (proc / str(pid)).mkdir(parents=True)
            (proc / str(pid) / "cmdline").write_bytes(b"\0".join(a.encode() for a in argv) + b"\0")
            (proc / str(pid) / "environ").write_bytes(b"HOME=/home/ubuntu\0")
        (proc / "self").mkdir()
        found = sorted(REAL_RUNNER_PROCESSES(proc))
        self.assertEqual([pid for pid, _, _ in found], [10, 11, 14])
        self.assertEqual(found[0][2], {"HOME": "/home/ubuntu"})

    def test_runner_on_the_same_filesystem_keeps_its_refusal_floor_before_any_mutation(self):
        c = config()
        (p.base_path(c, self.image)).mkdir(parents=True)
        for name in ("base.vmstate", "base.memory"):
            (p.base_path(c, self.image) / name).write_bytes(b"x")
        consumed = c["disk_quota_bytes"] + c["guest_ram_mib"] * 1048576
        state = self.root / "runner-state"
        floor = 128 * 1024**3
        # The lower-floor runner does not decide; the highest floor on the
        # filesystem does, and the refusal names its pid.
        runner = [(4241, ["motor-runner", "serve", "--state-root", str(state), "--free-space-floor-bytes", str(floor // 2)], None),
                  (4242, ["motor-runner", "serve", "--state-root", str(state)], None)]
        def allocate(*args):
            if args[0] == "fallocate":
                args[-1].write_bytes(b"allocated")
        execute = mock.Mock(side_effect=allocate)
        free = type("Usage", (), {"free": consumed + floor - 1})
        with mock.patch.object(p.shutil, "disk_usage", return_value=free), \
                self.assertRaisesRegex(ValueError, r"would leave 127\.9 GiB free .* below motor-runner pid 4242's refusal floor 128\.0 GiB"):
            p.provision(c, execute=execute, runners=runner)
        execute.assert_not_called()
        self.assertFalse((p.ROOT / "tenants" / c["tenant"]).exists())
        # A runner whose state is on another filesystem does not count.
        with mock.patch.object(p.shutil, "disk_usage", return_value=free), \
                mock.patch.object(p, "device", side_effect=lambda path: 1 if path == state else 2):
            p.provision(c, execute=execute, runners=runner)
        execute.reset_mock()
        c2 = dict(c, tenant="b", vm="vm-b", tap_index=2)
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": consumed + floor})):
            p.provision(c2, execute=execute, runners=runner)
        execute.assert_called()

    def test_existing_disk_is_never_overwritten(self):
        home = self.root / "tenants/a"
        home.mkdir()
        (home / "disk.img").write_bytes(b"keep")
        with self.assertRaisesRegex(ValueError, "already exists"):
            p.provision(config())
        self.assertEqual((home / "disk.img").read_bytes(), b"keep")

    def test_duplicate_tap_and_binding_are_rejected(self):
        (p.REGISTRY / "b.json").write_text('{"tap_index":1}')
        with self.assertRaisesRegex(ValueError, "tap index"):
            p.provision(config())
        (p.REGISTRY / "b.json").unlink()
        p.BINDINGS.write_text(json.dumps({"vm-a": {"tenant": "b", "sub": config()["sub"]}}))
        with self.assertRaisesRegex(ValueError, "binding"):
            p.provision(config())

    def test_image_hash_failure_precedes_allocation(self):
        (self.image / "rootfs").write_bytes(b"tampered")
        execute = mock.Mock()
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            p.provision(config(), execute=execute)
        execute.assert_not_called()

    def test_sizes_have_distinct_base_directories(self):
        a = config()
        b = copy.deepcopy(a)
        b.update(vcpu_ceiling=1, guest_ram_mib=8192, tap_index=2)
        self.assertEqual(p.base_path(a, self.image), self.image / "base/2-18432")
        self.assertEqual(p.base_path(b, self.image), self.image / "base/1-8192")

    def test_boot_args_are_the_exact_manifest_value(self):
        path = self.image / "manifest.json"
        manifest = json.loads(path.read_text())
        expected = "console=ttyS0 root=/dev/vda ro init=/sbin/motor-init net.ifnames=0"
        manifest["boot_args"] = expected
        path.write_text(json.dumps(manifest))
        self.assertEqual(p.boot_args(self.image), expected)
        for bad in ("", "x\ny", 123):
            manifest["boot_args"] = bad
            path.write_text(json.dumps(manifest))
            with self.assertRaises(ValueError):
                p.boot_args(self.image)

    def test_console_option_is_boolean_and_absent_on_normal_provisioning(self):
        self.assertNotIn("capture_console", config())
        value = config()
        value["capture_console"] = True
        self.assertTrue(p.checked(value)["capture_console"])
        value["capture_console"] = "true"
        with self.assertRaisesRegex(ValueError, "boolean"):
            p.checked(value)

    def test_qualification_payload_is_staged_only_during_initial_mount(self):
        events = []
        def execute(*args):
            events.append(args[0])
            if args[0] == "fallocate":
                args[-1].touch()
        def make_base(_c, _image, path):
            path.mkdir(parents=True)
        def stage(mount):
            self.assertEqual(events[-1], "mount")
            self.assertTrue((mount / "runner-args").is_file())
            events.append("stage")
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": 2**63})):
            p.provision(config(), make_base, execute, stage=stage)
        self.assertLess(events.index("mount"), events.index("stage"))
        self.assertLess(events.index("stage"), events.index("umount"))
        self.assertEqual((p.REGISTRY / "a.json").stat().st_mode & 0o777, 0o644)

    def agent_server(self, handler):
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.root / "vsock.sock"))
        listener.listen()
        listener.settimeout(3)
        errors = []
        def serve():
            try:
                with listener:
                    handler(listener)
            except BaseException as error:
                errors.append(error)
        thread = threading.Thread(target=serve)
        thread.start()
        self.addCleanup(lambda: thread.join(timeout=4))
        return thread, errors

    def receive_hello(self, conn):
        conn.settimeout(3)
        line = b""
        while not line.endswith(b"\n"):
            chunk = conn.recv(1)
            if not chunk:
                raise EOFError("fixture handshake closed")
            line += chunk
        self.assertEqual(line, b"CONNECT 54\n")

    def receive_control(self, conn):
        self.receive_hello(conn)
        conn.sendall(b"OK 54\n")
        def exact(size):
            result = b""
            while len(result) < size:
                chunk = conn.recv(size - len(result))
                if not chunk:
                    raise EOFError("fixture control frame closed")
                result += chunk
            return result
        size, = struct.unpack(">I", exact(4))
        return json.loads(exact(size))

    def test_initial_vsock_eof_retries_until_agent_is_really_listening(self):
        requests = []
        def serve(listener):
            conn, _ = listener.accept()
            with conn:
                self.receive_hello(conn)
            conn, _ = listener.accept()
            with conn:
                requests.append(self.receive_control(conn))
                reply = b'{"ok":true}'
                conn.sendall(struct.pack(">I", len(reply)) + reply)
        thread, errors = self.agent_server(serve)
        proc = mock.Mock()
        proc.poll.return_value = None
        record = mock.Mock()
        p.wait_for_base_agent(self.root, proc, record, timeout=3)
        thread.join(timeout=4)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(requests, [{"op": "prepare_snapshot", "kind": "base"}])
        self.assertEqual([call.args[0] for call in record.call_args_list], ["agent_not_ready", "agent_ready"])
        proc.kill.assert_not_called()

    def test_eof_after_control_submission_is_fatal_and_never_retried(self):
        requests = []
        def serve(listener):
            conn, _ = listener.accept()
            with conn:
                requests.append(self.receive_control(conn))
        thread, errors = self.agent_server(serve)
        proc = mock.Mock()
        proc.poll.return_value = None
        probe = mock.Mock(wraps=p.agent)
        with self.assertRaisesRegex(EOFError, "after request submission"):
            p.wait_for_base_agent(self.root, proc, mock.Mock(), probe=probe)
        thread.join(timeout=4)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(len(requests), 1)
        self.assertEqual(probe.call_count, 1)

    def test_readiness_timeout_and_early_vmm_exit_name_the_cause(self):
        proc = mock.Mock()
        proc.poll.return_value = None
        probe = mock.Mock(side_effect=p.AgentNotReady("listener absent"))
        with self.assertRaisesRegex(TimeoutError, "60s while VMM was running.*listener absent"):
            p.wait_for_base_agent(self.root, proc, mock.Mock(), probe=probe, clock=mock.Mock(side_effect=[0, 60]))
        proc.poll.return_value = -9
        probe.reset_mock()
        with self.assertRaisesRegex(RuntimeError, "returncode=-9"):
            p.wait_for_base_agent(self.root, proc, mock.Mock(), probe=probe)
        probe.assert_not_called()

    def test_real_guest_refusal_is_not_a_readiness_retry(self):
        proc = mock.Mock()
        proc.poll.return_value = None
        probe = mock.Mock(side_effect=RuntimeError("guest refused base preparation: busy"))
        with self.assertRaisesRegex(RuntimeError, "guest refused"):
            p.wait_for_base_agent(self.root, proc, mock.Mock(), probe=probe)
        self.assertEqual(probe.call_count, 1)

    def test_boot_and_diagnostic_requests_match_published_firecracker_schema(self):
        args = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro init=/sbin/motor-init"
        requests = p.base_requests(config(), args)
        actual = dict(requests)
        self.assertEqual(actual["/boot-source"]["boot_args"], args)
        self.assertEqual(actual["/drives/rootfs"], dict(drive_id="rootfs", path_on_host="/rootfs.ext4", is_root_device=True, is_read_only=True))
        self.assertFalse(actual["/drives/tenant"]["is_root_device"])
        for path, body in requests + [("/logger", dict(log_path="/state/firecracker.log", level="Debug", show_level=True, show_log_origin=True)),
                                      ("/metrics", dict(metrics_path="/state/metrics.jsonl")),
                                      ("/actions", dict(action_type="FlushMetrics"))]:
            p.SCHEMA.validate("PUT", path, body)
        p.SCHEMA.validate("GET", "/vm/config", None)

    def test_boot_evidence_preserves_config_metrics_exit_and_host_kill_reason(self):
        evidence = p.BootEvidence(self.root / "diagnostics")
        state, group = self.root / "state", self.root / "group"
        state.mkdir()
        group.mkdir()
        (state / "firecracker.log").write_text("raw Firecracker failure\n")
        (state / "metrics.jsonl").write_text('{"vmm":{}}\n')
        (group / "memory.events").write_text("oom 0\noom_kill 0\n")
        evidence.record("api_request", method="PUT", path="/boot-source", body=dict(boot_args="manifest args"))
        proc = mock.Mock(pid=123)
        proc.poll.return_value = None
        proc.wait.return_value = -9
        api = mock.Mock(return_value=dict(boot_source=dict(boot_args="observed args")))
        evidence.stop(proc, group, state, api, EOFError("before handshake"))
        events = [json.loads(line) for line in (evidence.directory / "events.jsonl").read_text().splitlines()]
        before = next(row for row in events if row["kind"] == "vmm_before_cleanup")
        self.assertIsNone(before["returncode"])
        self.assertEqual(before["reason"], "EOFError")
        self.assertEqual(next(row for row in events if row["kind"] == "host_kill")["reason"], "base_boot_cleanup")
        self.assertEqual(events[-1]["returncode"], -9)
        self.assertEqual((evidence.directory / "firecracker.log").read_text(), "raw Firecracker failure\n")
        self.assertEqual((evidence.directory / "metrics.jsonl").read_text(), '{"vmm":{}}\n')
        self.assertEqual(json.loads((evidence.directory / "submitted-config.json").read_text())["/boot-source"]["boot_args"], "manifest args")
        self.assertTrue((evidence.directory / "observed-config.json").exists())
        api.assert_any_call("PUT", "/actions", {"action_type": "FlushMetrics"}, timeout=1)
        proc.kill.assert_called_once()

    def test_already_exited_vmm_records_status_without_claiming_a_host_kill(self):
        evidence = p.BootEvidence(self.root / "diagnostics")
        proc = mock.Mock(pid=123)
        proc.poll.return_value = 137
        proc.wait.return_value = 137
        api = mock.Mock()
        evidence.stop(proc, self.root / "absent-group", self.root, api, RuntimeError("exit"))
        events = [json.loads(line) for line in (evidence.directory / "events.jsonl").read_text().splitlines()]
        self.assertEqual(events[0]["returncode"], 137)
        self.assertNotIn("host_kill", [row["kind"] for row in events])
        proc.kill.assert_not_called()
        api.assert_not_called()


if __name__ == "__main__":
    unittest.main(verbosity=2)
