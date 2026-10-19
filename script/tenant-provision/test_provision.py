#!/usr/bin/env python3
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("provision", Path(__file__).with_name("provision.py"))
assert spec is not None and spec.loader is not None
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


def config():
    return p.checked(dict(tenant="a", sub="acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", vm="vm-a", image_version="v1", vcpu_ceiling=2,
                          guest_ram_mib=18432, disk_quota_bytes=21474836480, cpuset="0-1", tap_index=1, idle_suspend_s=60,
                          host_floor_bytes=2147483648, resolvers=["1.1.1.1"]))


class ProvisionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.patch = mock.patch.multiple(p, ROOT=self.root, REGISTRY=self.root / "registry", BINDINGS=self.root / "host/tenants.json")
        self.patch.start()
        for directory in [p.REGISTRY, p.BINDINGS.parent, p.ROOT / "tenants"]:
            directory.mkdir(parents=True)
        self.image = self.root / "tenant-images/v1"
        self.image.mkdir(parents=True)
        (self.image / "vmlinux").write_bytes(b"kernel")
        (self.image / "rootfs").write_bytes(b"root")
        (self.image / "manifest.json").write_text(json.dumps(dict(version="v1", kernel_sha256=hashlib.sha256(b"kernel").hexdigest(), rootfs_sha256=hashlib.sha256(b"root").hexdigest())))

    def tearDown(self):
        self.patch.stop()
        self.tmp.cleanup()

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
