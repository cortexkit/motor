#!/usr/bin/env python3
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import socket
import stat
import struct
import tempfile
import threading
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("provision", Path(__file__).with_name("provision.py"))
assert spec is not None and spec.loader is not None
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
capacity_spec = importlib.util.spec_from_file_location("test_capacity", Path(__file__).with_name("test_capacity.py"))
assert capacity_spec is not None and capacity_spec.loader is not None
capacity_tests = importlib.util.module_from_spec(capacity_spec)
capacity_spec.loader.exec_module(capacity_tests)
resize_spec = importlib.util.spec_from_file_location("test_resize", Path(__file__).with_name("test_resize.py"))
assert resize_spec is not None and resize_spec.loader is not None
resize_tests = importlib.util.module_from_spec(resize_spec)
resize_spec.loader.exec_module(resize_tests)
# The script-test runner enters through this file, including the capacity and
# resize suites.
CapacityTests = capacity_tests.CapacityTests
ResizeTests = resize_tests.ResizeTests
# Kept before setUp replaces it, for the one test that reads a fake /proc.
REAL_RUNNER_PROCESSES = p.runner_processes
# The real host paths, kept before setUp points them into a scratch directory.
LIVE_PATHS = dict(ROOT=p.ROOT, REGISTRY=p.REGISTRY, PROVISION=p.PROVISION, BINDINGS=p.BINDINGS)
# The hosted-host renderer whose output install.sh requires the installed
# provisioning record to match byte for byte.
hosted_spec = importlib.util.spec_from_file_location(
    "hosted_host", Path(__file__).resolve().parents[1] / "hosted-host" / "hosted_host.py")
assert hosted_spec is not None and hosted_spec.loader is not None
hosted = importlib.util.module_from_spec(hosted_spec)
hosted_spec.loader.exec_module(hosted)
# The host's two records as found: the provisioning record still names the
# image from before a hand swap; the helper registry names the image running.
RECORD_IMAGE = "tenant-20261020-main-806a6d3-20261020T021255Z"
REGISTRY_IMAGE = "tenant-20261020-net-bb3e7fd-20261020T163144Z"
SUB = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV"


def config():
    return p.checked(dict(tenant="a", sub="acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", vm="vm-a", image_version="v1", vcpu_ceiling=2,
                          guest_ram_mib=18432, disk_quota_bytes=21474836480, cpuset="0-1", tap_index=1, idle_suspend_s=60,
                          host_floor_bytes=p.RUNNER_DEFAULT_FLOOR, resolvers=["1.1.1.1"]))


class ProvisionTests(unittest.TestCase):
    @staticmethod
    def owned_tap(tap):
        return {'ifname': tap, 'linkinfo': {'info_kind': 'tun', 'info_data': {
            'type': 'tap', 'persist': True, 'user': 400000 + int(tap.removeprefix('mtap'))}}}

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.reset_counter = mock.Mock()
        # runner_processes would read this machine's real /proc; tests that need
        # a runner pass one explicitly.
        self.patch = mock.patch.multiple(p, ROOT=self.root, REGISTRY=self.root / "registry", PROVISION=self.root / "provision",
                                          BINDINGS=self.root / "host/tenants.json",
                                          JOBS_DROP_IN=self.root / "jobs.conf",
                                          read_capacity=lambda: {"windows_vm_bytes": 16 * 1024**3,
                                                                 "host_reserve_bytes": 6 * 1024**3,
                                                                 "jobs_min_bytes": 24 * 1024**3,
                                                                 "storage_margin_bytes": 8 * 1024**3,
                                                                 "growth_allowance_bytes": 64 * 1024**3,
                                                                 "tenant_memory_reserve_bytes": 2 * 1024**3},
                                          memtotal_bytes=lambda: 128 * 1024**3,
                                          slice_properties=lambda _names: {"LoadState": "loaded", "MemoryMax": "42949672960"},
                                          runner_processes=lambda: [], reset_egress_counter=self.reset_counter)
        self.patch.start()
        p.JOBS_DROP_IN.write_text("[Slice]\nMemoryMax=42949672960\nMemoryHigh=35791394134\n")
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

    def removal_records(self):
        """Two provisioned tenants, a (tap 1) and b (tap 2), with disks,
        suspend snapshots, a set-aside stale snapshot and bindings."""
        bindings = {}
        for tenant, tap in (("a", 1), ("b", 2)):
            c = p.checked({**config(), "tenant": tenant, "vm": f"vm-{tenant}", "tap_index": tap})
            (p.PROVISION / f"{tenant}.json").write_text(json.dumps(c, indent=2) + "\n")
            home = p.ROOT / "tenants" / tenant
            record = {key: value for key, value in c.items() if key not in p.PROVISION_ONLY}
            record.update({name: str(home / leaf) for name, leaf in p.REGISTRY_PATHS.items()})
            (p.REGISTRY / f"{tenant}.json").write_text(json.dumps(record, indent=2) + "\n")
            (home / "suspend").mkdir(parents=True)
            (home / "suspend.stale-v0-1").mkdir()
            (home / "disk.img").write_bytes(tenant.encode())
            (home / "generation").write_text("3\n")
            (home / "suspend/suspend.memory").write_bytes(b"memory")
            (home / "suspend/key.json").write_text("{}")
            bindings[f"vm-{tenant}"] = {"tenant": tenant, "sub": c["sub"]}
        p.BINDINGS.write_text(json.dumps(bindings, indent=2) + "\n")
        p.BINDINGS.chmod(0o644)

    def snapshot_tree(self):
        return {str(path.relative_to(self.root)): path.read_bytes() if path.is_file() else None
                for path in sorted(self.root.rglob("*")) if path.name != "provision.lock"}

    def test_removal_layout_is_the_one_the_helper_removes(self):
        pinned = json.loads(Path(__file__).with_name("removal-layout.json").read_text())
        with mock.patch.multiple(p, **LIVE_PATHS):
            self.assertEqual(p.removal_layout(), pinned)

    def test_remove_tenant_refuses_while_either_service_is_running(self):
        self.removal_records()
        before = self.snapshot_tree()
        for active in ("motor-host.service", "motor-vm-helper.service"):
            with self.subTest(active=active):
                states = {"motor-host.service": "inactive", "motor-vm-helper.service": "inactive", active: "active"}
                execute = mock.Mock()
                with self.assertRaisesRegex(ValueError, "must be stopped") as refused:
                    p.remove_tenant("a", check_service=states.get, execute=execute, has_tap=lambda _tap: True,
                                    output=lambda _line: None)
                self.assertIn(p.STOP_COMMAND, str(refused.exception))
                execute.assert_not_called()
                self.assertEqual(self.snapshot_tree(), before)

    def test_remove_tenant_dry_run_lists_everything_and_deletes_nothing(self):
        self.removal_records()
        before = self.snapshot_tree()
        lines = []
        execute = mock.Mock()
        p.remove_tenant("a", dry_run=True, check_service=lambda _unit: "inactive", execute=execute,
                        has_tap=lambda tap: tap == "mtap1", now=lambda: 1000, output=lines.append, tap_info=self.owned_tap)
        execute.assert_not_called()
        self.assertEqual(self.snapshot_tree(), before)
        listed = "\n".join(lines)
        home = p.ROOT / "tenants/a"
        for path in (home / "disk.img", home / "generation", home / "suspend/suspend.memory",
                     home / "suspend/key.json", home / "suspend.stale-v0-1", p.REGISTRY / "a.json", p.PROVISION / "a.json"):
            self.assertIn(f"{path}\n", listed + "\n")
        self.assertIn(f"binding vm-a in {p.BINDINGS}", listed)
        self.assertIn("delete: tap mtap1", listed)
        self.assertIn("ufw route motor-tenant-mtap1", listed)
        self.assertTrue(all(line.startswith("would ") for line in lines), lines)
        self.assertNotIn("tenants/b", listed)
        self.assertNotIn("mtap2", listed)

    def test_remove_tenant_quarantines_one_tenant_and_the_second_survives(self):
        self.removal_records()
        b_before = {path: data for path, data in self.snapshot_tree().items()
                    if path.startswith("tenants/b") or path.endswith("b.json")}
        execute = mock.Mock()
        messages = []
        p.remove_tenant("a", check_service=lambda _unit: "inactive", execute=execute,
                        has_tap=lambda tap: tap in ("mtap1", "mtap2"), now=lambda: 1000, output=messages.append, tap_info=self.owned_tap)
        execute.assert_called_once_with("ip", "link", "delete", "dev", "mtap1")
        quarantine = p.ROOT / "removed/a-1000"
        self.assertEqual((quarantine / "complete").read_text(), "1\n")
        self.assertFalse((p.ROOT / "tenants/a").exists())
        self.assertFalse((p.REGISTRY / "a.json").exists())
        self.assertFalse((p.PROVISION / "a.json").exists())
        self.assertEqual((quarantine / "home/disk.img").read_bytes(), b"a")
        self.assertTrue((quarantine / "home/suspend/suspend.memory").exists())
        self.assertTrue((quarantine / "home/suspend.stale-v0-1").is_dir())
        self.assertEqual(json.loads((quarantine / "records/registry.json").read_text())["tap_index"], 1)
        self.assertEqual(json.loads((quarantine / "records/provision.json").read_text())["tenant"], "a")
        self.assertEqual(json.loads((quarantine / "records/binding.json").read_text()),
                         {"vm-a": {"tenant": "a", "sub": SUB}})
        self.assertEqual(stat.S_IMODE(quarantine.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((p.ROOT / "removed").stat().st_mode), 0o700)
        self.assertEqual(json.loads(p.BINDINGS.read_text()), {"vm-b": {"tenant": "b", "sub": SUB}})
        self.assertEqual(stat.S_IMODE(p.BINDINGS.stat().st_mode), 0o644)
        b_after = {path: data for path, data in self.snapshot_tree().items()
                   if path.startswith("tenants/b") or path.endswith("b.json")}
        self.assertEqual(b_after, b_before)
        self.assertTrue(any("quarantined" in line for line in messages), messages)
        # The tenant is gone: a second removal finds nothing to remove.
        with self.assertRaisesRegex(ValueError, "no helper registry entry"):
            p.remove_tenant("a", check_service=lambda _unit: "inactive", execute=execute,
                            has_tap=lambda _tap: False, output=lambda _line: None)

    def test_remove_tenant_purge_now_deletes_instead_of_quarantining(self):
        self.removal_records()
        p.remove_tenant("a", purge_now=True, check_service=lambda _unit: "inactive", execute=mock.Mock(),
                        has_tap=lambda _tap: False, now=lambda: 1000, output=lambda _line: None)
        self.assertFalse((p.ROOT / "tenants/a").exists())
        self.assertEqual(list((p.ROOT / "removed").iterdir()), [])

    def test_remove_tenant_without_home_marks_journal_complete(self):
        self.removal_records()
        shutil.rmtree(p.ROOT / 'tenants/a')
        p.remove_tenant('a', check_service=lambda _: 'inactive', execute=mock.Mock(),
                        has_tap=lambda _: False, now=lambda: 1000, output=lambda _: None)
        quarantine = p.ROOT / 'removed/a-1000'
        self.assertEqual((quarantine / 'complete').read_text(), '1\n')
        self.assertFalse((quarantine / 'home').exists())
        self.assertFalse((p.REGISTRY / 'a.json').exists())

    def test_remove_tenant_refuses_foreign_tap_before_any_mutation(self):
        self.removal_records()
        before = self.snapshot_tree()
        for field, value in [('user', 400002), ('type', 'tun'), ('persist', False)]:
            with self.subTest(field=field):
                link = self.owned_tap('mtap1')
                link['linkinfo']['info_data'][field] = value
                execute = mock.Mock()
                with self.assertRaisesRegex(ValueError, 'foreign interface mtap1'):
                    p.remove_tenant('a', check_service=lambda _: 'inactive', execute=execute,
                                    has_tap=lambda _: True, tap_info=lambda _: link, output=lambda _: None)
                execute.assert_not_called()
                self.assertEqual(self.snapshot_tree(), before)
        link = self.owned_tap('mtap1')
        link['linkinfo']['info_kind'] = 'dummy'
        with self.assertRaisesRegex(ValueError, 'foreign interface mtap1'):
            p.remove_tenant('a', check_service=lambda _: 'inactive', has_tap=lambda _: True,
                            tap_info=lambda _: link, execute=lambda *_: self.fail('foreign delete'))
        self.assertEqual((p.ROOT / "tenants/b/disk.img").read_bytes(), b"b")

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
                with self.assertRaisesRegex(ValueError, "must be stopped") as refused:
                    p.switch_image("a", "v2", make_base=make_base, check_service=states.get,
                                   validate_image=validate, output=lambda _line: None)
                # The refusal names the exact command that stops the tenant
                # cleanly, not a bare kill of the VM.
                self.assertIn("sudo systemctl stop motor-host.service motor-vm-helper.service", str(refused.exception))
                self.assertIn("cleanly", str(refused.exception))
                validate.assert_not_called()
                make_base.assert_not_called()
                self.assertEqual(provision_path.read_bytes(), old_config)
                self.assertEqual(registry_path.read_bytes(), old_record)

    def test_switch_image_warns_when_the_last_vm_stop_was_unclean(self):
        self.switch_records()
        home = p.ROOT / "tenants/a"
        messages = []
        switch = lambda: p.switch_image("a", "v2", make_base=self.fake_base, check_service=lambda _unit: "inactive",
                                        validate_image=lambda _image: None, output=messages.append)
        switch()
        self.assertFalse(any("did not stop cleanly" in line for line in messages), messages)
        # The helper's running marker outlived the stop: the guest's power was
        # cut, and the operator is told that its next start runs recovery.
        (home / "running.json").write_text("{}")
        messages.clear()
        switch()
        warning = [line for line in messages if "did not stop cleanly" in line]
        self.assertEqual(len(warning), 1, messages)
        self.assertIn("recovery", warning[0])
        # The switch itself does not touch the marker; the helper turns it into
        # the unclean-stop record when the tenant next starts.
        self.assertTrue((home / "running.json").exists())

    def test_switch_image_runs_the_hosted_image_integrity_command(self):
        # `verify`, not `inventory`: a rollback switches back to an installed
        # image whose pins rightly differ from today's versions.txt.
        with mock.patch.object(p.subprocess, "run") as execute:
            p.inventory_image(self.image)
        command = execute.call_args.args[0]
        self.assertEqual(command[1:], [str(Path(__file__).resolve().parents[1] / "tenant-image" / "build.py"),
                                       "verify", str(self.image)])
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

    def installer_render(self, image_version):
        """The bytes install.sh installs, and compares, for tenant-1's record."""
        with mock.patch.object(hosted, "load_provision", return_value=p):
            return json.dumps(hosted.tenant_config(SUB, 100, image_version), indent=2) + "\n"

    def add_image(self, version):
        image = self.root / "tenant-images" / version
        image.mkdir(parents=True)
        (image / "vmlinux").write_bytes(b"kernel-" + version.encode())
        (image / "rootfs").write_bytes(b"root-" + version.encode())
        (image / "manifest.json").write_text(json.dumps(dict(
            version=version, kernel_sha256=hashlib.sha256(b"kernel-" + version.encode()).hexdigest(),
            rootfs_sha256=hashlib.sha256(b"root-" + version.encode()).hexdigest())))
        return image

    def mismatched_tenant_1(self):
        """Records of the live mismatch, each written the way its writer writes it."""
        provision_path = p.PROVISION / "tenant-1.json"
        provision_path.write_text(self.installer_render(RECORD_IMAGE))
        provision_path.chmod(0o644)
        c = p.checked(json.loads(provision_path.read_text()))
        record = {key: value for key, value in c.items() if key not in {"tenant", "sub", "vm", "resolvers"}}
        home = p.ROOT / "tenants/tenant-1"
        record.update(disk=str(home / "disk.img"), generation=str(home / "generation"), suspend=str(home / "suspend"),
                      image_version=REGISTRY_IMAGE)
        registry_path = p.REGISTRY / "tenant-1.json"
        registry_path.write_text(json.dumps(record, indent=2) + "\n")
        (home / "suspend").mkdir(parents=True)
        self.add_image(RECORD_IMAGE)
        self.add_image(REGISTRY_IMAGE)
        return provision_path, registry_path

    def repair(self, validate_image=lambda _image: None, output=None):
        return p.repair_from_registry("tenant-1", validate_image=validate_image, check_trusted=lambda _path: None,
                                      output=(lambda _line: None) if output is None else output)

    def test_switch_image_refuses_the_live_mismatch_until_it_is_repaired(self):
        provision_path, registry_path = self.mismatched_tenant_1()
        target = "tenant-20261021-main-0000000-20261021T120000Z"
        self.add_image(target)
        switch = lambda: p.switch_image("tenant-1", target, make_base=self.fake_base, check_service=lambda _unit: "inactive",
                                        validate_image=lambda _image: None, output=lambda _line: None)
        with self.assertRaisesRegex(ValueError, "image versions differ"):
            switch()
        registry_before = registry_path.read_bytes()
        validated = []
        messages = []
        self.assertTrue(self.repair(validate_image=validated.append, output=messages.append))

        # Exactly image_version changed, to the registry's, and nothing else.
        self.assertEqual(validated, [self.root / "tenant-images" / REGISTRY_IMAGE])
        self.assertEqual(registry_path.read_bytes(), registry_before)
        self.assertEqual(json.loads(provision_path.read_text()),
                         {**json.loads(self.installer_render(RECORD_IMAGE)), "image_version": REGISTRY_IMAGE})
        self.assertEqual(messages, [f"{provision_path}: image_version {RECORD_IMAGE} -> {REGISTRY_IMAGE} "
                                    f"(from {registry_path}); no other field changed"])
        # install.sh --check accepts it: the bytes equal its render for the
        # installed image, the mode is unchanged and the images now agree.
        self.assertEqual(provision_path.read_text(), self.installer_render(REGISTRY_IMAGE))
        self.assertEqual(provision_path.stat().st_mode & 0o777, 0o644)
        self.assertEqual(json.loads(registry_path.read_text())["image_version"],
                         json.loads(provision_path.read_text())["image_version"])
        self.assertEqual(list(p.PROVISION.glob("*.pending")), [])

        # Repairing again changes nothing; the switch now proceeds.
        messages.clear()
        self.assertFalse(self.repair(output=messages.append))
        self.assertIn("nothing changed", messages[0])
        switch()
        self.assertEqual(json.loads(provision_path.read_text())["image_version"], target)
        self.assertEqual(json.loads(registry_path.read_text())["image_version"], target)

    def test_repair_refuses_without_changing_anything_unless_only_the_image_differs(self):
        def drop(record, name):
            del record[name]
        cases = {
            "registry image missing": (None, "registry image does not exist"),
            "registry image fails validation": (None, "inventory failed"),
            "registry manifest names another version": (None, "manifest version mismatch"),
            "other field differs": (lambda record: record.update(vcpu_ceiling=7), "differ at vcpu_ceiling"),
            "disk path differs": (lambda record: record.update(disk="/elsewhere/disk.img"), "differ at disk"),
            "registry field missing": (lambda record: drop(record, "io_weight"), "differ at io_weight"),
            "registry field extra": (lambda record: record.update(capture_console=True), "differ at capture_console"),
            "registry image not a name": (lambda record: record.update(image_version="../x"), "path component"),
        }
        for case, (edit, message) in cases.items():
            with self.subTest(case=case):
                self.tearDown()
                self.setUp()
                provision_path, registry_path = self.mismatched_tenant_1()
                if edit is not None:
                    record = json.loads(registry_path.read_text())
                    edit(record)
                    registry_path.write_text(json.dumps(record, indent=2) + "\n")
                validate = lambda _image: None
                if case == "registry image missing":
                    shutil.rmtree(self.root / "tenant-images" / REGISTRY_IMAGE)
                elif case == "registry image fails validation":
                    def validate(_image):
                        raise ValueError("inventory failed")
                elif case == "registry manifest names another version":
                    (self.root / "tenant-images" / REGISTRY_IMAGE / "manifest.json").write_text(json.dumps(dict(version=RECORD_IMAGE)))
                provision_before, registry_before = provision_path.read_bytes(), registry_path.read_bytes()
                with self.assertRaisesRegex(ValueError, message):
                    self.repair(validate_image=validate)
                self.assertEqual(provision_path.read_bytes(), provision_before)
                self.assertEqual(registry_path.read_bytes(), registry_before)

    def test_repair_keeps_the_old_record_if_the_replace_fails(self):
        provision_path, _registry_path = self.mismatched_tenant_1()
        before = provision_path.read_bytes()
        with mock.patch.object(p.os, "replace", side_effect=OSError("injected replace failure")), \
                self.assertRaisesRegex(OSError, "injected"):
            self.repair()
        self.assertEqual(provision_path.read_bytes(), before)
        self.assertEqual(list(p.PROVISION.glob("*.pending")) + list(p.PROVISION.glob(".*.pending")), [])

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

    def test_new_tenant_reserves_windows_threads_only_with_a_windows_vm(self):
        c = dict(config(), cpuset="12-15,28-31", vcpu_ceiling=8)
        capacity = dict(p.read_capacity(), windows_vm_bytes=1)

        def execute(*args):
            if args[0] == "fallocate":
                args[-1].write_bytes(b"allocated")

        def base(_c, _image, path):
            path.mkdir(parents=True)

        for windows in (1, 0):
            capacity["windows_vm_bytes"] = windows
            p.JOBS_DROP_IN.write_text(p.jobs_slice_render(capacity))
            before = sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*"))
            with self.subTest(windows_vm_bytes=windows), mock.patch.object(p, "read_capacity", return_value=capacity) as read, \
                    mock.patch.object(p, "slice_properties", return_value={"LoadState": "loaded", "MemoryMax": str(24 * 1024**3 + windows)}), \
                    mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": 2**63})):
                if windows:
                    with self.assertRaisesRegex(ValueError, "overlaps the Windows VM's CPUs"):
                        p.provision(c, base, execute)
                    self.assertEqual(before, sorted(str(path.relative_to(self.root)) for path in self.root.rglob("*")))
                else:
                    p.provision(c, base, execute)
                    self.assertEqual(json.loads((p.REGISTRY / "a.json").read_text())["cpuset"], c["cpuset"])
                read.assert_called_once_with()

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
        needed = p.pre_operation_free_bytes(c, p.read_capacity(), True)
        # One byte short of the host requirement is refused before the runner
        # is consulted at all.
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": needed - 1})), \
                self.assertRaisesRegex(ValueError, f"provisioning needs {needed} free bytes"):
            p.provision(c, execute=execute, runners=runner)
        execute.assert_not_called()
        self.assertFalse((p.ROOT / "tenants" / c["tenant"]).exists())
        # With the runner's state outside the tenant's filesystem, the same
        # host passes.
        ample = needed + floor
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": ample})), \
                mock.patch.object(p, "device", side_effect=lambda path: 1 if path == state else 2):
            p.provision(c, execute=execute, runners=runner)
        execute.reset_mock()
        c2 = dict(c, tenant="b", vm="vm-b", tap_index=2)
        # Sharing the filesystem admits the tenant too, with room for its
        # requirement and the runner's floor on top.
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": ample + floor})):
            p.provision(c2, execute=execute, runners=runner)
        execute.assert_called()

    def test_pre_operation_requirement_passes_at_exact_and_refuses_one_byte_short(self):
        index = [0]
        for base, idle in ((False, 60), (True, 60), (False, 0), (True, 0)):
            with self.subTest(base=base, idle=idle):
                number = index[0]
                index[0] += 1
                c = p.checked({**config(), "tenant": f"exact{number}", "vm": f"vm-exact{number}",
                               "tap_index": 20 + number, "idle_suspend_s": idle})
                base_dir = p.base_path(c, self.image)
                if base:
                    base_dir.mkdir(parents=True, exist_ok=True)
                    for name in ("base.vmstate", "base.memory"):
                        (base_dir / name).write_bytes(b"base")
                else:
                    for name in ("base.vmstate", "base.memory"):
                        (base_dir / name).unlink(missing_ok=True)
                    if base_dir.exists():
                        base_dir.rmdir()
                self.assertEqual(p.base_present(c, self.image), base)
                needed = p.pre_operation_free_bytes(c, p.read_capacity(), base)
                def allocate(*args):
                    if args[0] == "fallocate":
                        with args[-1].open("wb") as file:
                            file.truncate(args[2])
                execute = mock.Mock(side_effect=allocate)
                with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": needed - 1})), \
                        self.assertRaisesRegex(ValueError, f"provisioning needs {needed} free bytes"):
                    p.provision(c, execute=execute)
                execute.assert_not_called()
                self.assertFalse((p.ROOT / "tenants" / c["tenant"]).exists())
                make_base = mock.Mock(side_effect=self.fake_base)
                with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": needed})):
                    p.provision(c, execute=execute, make_base=make_base)
                execute.assert_called()
                self.assertEqual(make_base.call_count, 0 if base else 1)

    def test_permits_come_from_max_weight_and_are_never_injected(self):
        keyless = config()
        self.assertNotIn("max_weight", keyless)
        self.assertIn("--max-weight 2 --rustc-slots 2", p.runner_args(keyless))
        record = {name: value for name, value in keyless.items() if name not in p.PROVISION_ONLY}
        self.assertNotIn("max_weight", record)
        # The operator's budget reaches both runner arguments and the helper
        # registry, and the two arguments stay equal for the guest's check.
        c = p.checked({**config(), "max_weight": 64, "vcpu_ceiling": 24})
        self.assertIn("--max-weight 64 --rustc-slots 64", p.runner_args(c))
        self.assertEqual(c["max_weight"], 64)
        # A value the guest's u32 parser cannot read, one below the ceiling and
        # a non-integer are all refused.
        for value in (2**32, 2**32 - 1 + 1, 23, 0, -1, True, "64", 1.5):
            with self.subTest(value=value), self.assertRaises(ValueError):
                p.checked({**config(), "max_weight": value, "vcpu_ceiling": 24})
        # The largest u32 is accepted, and idle 0 provisions.
        self.assertEqual(p.checked({**config(), "max_weight": 2**32 - 1, "vcpu_ceiling": 24})["max_weight"], 2**32 - 1)
        self.assertEqual(p.checked({**config(), "idle_suspend_s": 0})["idle_suspend_s"], 0)
        with self.assertRaisesRegex(ValueError, "idle_suspend_s"):
            p.checked({**config(), "idle_suspend_s": -1})

    def test_a_sparse_v1_image_truncates_and_other_images_fallocate(self):
        for index, (capabilities, allocated) in enumerate([(["sparse-v1"], False), ([], True), (None, True)]):
            with self.subTest(capabilities=capabilities):
                manifest = dict(version="v1", kernel_sha256=hashlib.sha256(b"kernel").hexdigest(),
                                rootfs_sha256=hashlib.sha256(b"root").hexdigest())
                if capabilities is not None:
                    manifest["capabilities"] = capabilities
                (self.image / "manifest.json").write_text(json.dumps(manifest))
                calls = []
                def execute(*args):
                    calls.append(args[0])
                    if args[0] == "fallocate":
                        # A real fallocate gives the file the quota's length.
                        with args[-1].open("wb") as file:
                            file.truncate(args[2])
                c = p.checked({**config(), "tenant": f"sparse{index}", "vm": f"vm-sparse{index}", "tap_index": 10 + index})
                base = p.base_path(c, self.image)
                base.mkdir(parents=True, exist_ok=True)
                for name in ("base.vmstate", "base.memory"):
                    (base / name).write_bytes(b"base")
                with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": p.pre_operation_free_bytes(c, p.read_capacity(), True)})):
                    p.provision(c, execute=execute)
                disk = p.ROOT / "tenants" / c["tenant"] / "disk.img"
                self.assertEqual(disk.stat().st_size, c["disk_quota_bytes"])
                self.assertEqual("fallocate" in calls, allocated)
                self.assertEqual(calls[:2] if allocated else calls[:1], ["fallocate", "mkfs.xfs"] if allocated else ["mkfs.xfs"])

    def test_base_drive_carries_discard_and_a_protected_vmm(self):
        requests = dict(p.base_requests(config(), "console=ttyS0"))
        self.assertEqual(requests["/drives/tenant"]["discard"], True)
        self.assertEqual(requests["/drives/tenant"]["io_engine"], "Sync")
        p.SCHEMA.validate("PUT", "/drives/tenant", requests["/drives/tenant"])
        scratch = self.root / "proc"
        scratch.mkdir()
        (scratch / "comm").write_text("firecracker\n")
        value = self.root / "oom_score_adj"
        self.assertEqual(p.protect_vmm(123, proc=scratch, oom=scratch)["oom_score_adj"], -500)
        self.assertEqual((scratch / "oom_score_adj").read_text(), "-500")
        # A wrong process, a failed write and a wrong read-back all refuse the
        # boot instead of leaving an unprotected VMM running.
        # Right after spawn the jailer may not have exec'd Firecracker yet;
        # the score set on it survives the exec.
        (scratch / "comm").write_text("jailer\n")
        self.assertEqual(p.protect_vmm(123, proc=scratch, oom=scratch)["comm"], "jailer")
        (scratch / "comm").write_text("bash\n")
        with self.assertRaisesRegex(RuntimeError, "expected jailer or firecracker"):
            p.protect_vmm(123, proc=scratch, oom=scratch)
        (scratch / "comm").write_text("firecracker\n")
        with mock.patch.object(Path, "write_text", side_effect=OSError("injected")), \
                self.assertRaisesRegex(RuntimeError, "cannot set the VMM oom_score_adj"):
            p.protect_vmm(123, proc=scratch, oom=scratch)
        with self.assertRaisesRegex(RuntimeError, "oom readback"):
            p.protect_vmm(123, proc=scratch, oom=scratch, reread=lambda: "0\n")

    def test_protect_vmm_reads_the_pids_own_proc_directory_by_default(self):
        # Without overrides it must look in /proc/<pid>, not /proc itself.
        # A real base build reached `/proc/comm` and failed before this fix.
        seen = []
        original = Path.read_text

        def read_text(path, *args, **kwargs):
            seen.append(str(path))
            if str(path) == "/proc/4242/comm":
                return "firecracker\n"
            return original(path, *args, **kwargs)

        with mock.patch.object(Path, "read_text", read_text), \
                mock.patch.object(Path, "write_text", return_value=None):
            result = p.protect_vmm(4242, reread=lambda: "-500\n")
        self.assertEqual(seen, ["/proc/4242/comm"])
        self.assertEqual(result, dict(pid=4242, comm="firecracker", oom_score_adj=-500))

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

    def test_index_reuse_resets_egress_on_removal_and_before_replacement_publication(self):
        self.removal_records()
        counters = {1: 12345, 2: 987}
        def reset(index):
            self.assertFalse((p.REGISTRY / 'c.json').exists())
            counters[index] = 0
        self.reset_counter.side_effect = reset
        p.remove_tenant('a', check_service=lambda _: 'inactive', execute=mock.Mock(),
                        has_tap=lambda _: False, now=lambda: 1000, output=lambda _: None)
        self.assertEqual(counters, {1: 0, 2: 987})
        counters[1] = 45678  # Deferred state from an older removal implementation.
        replacement = config()
        replacement.update(tenant='c', vm='vm-c')
        def execute(*args):
            if args[0] == 'fallocate':
                args[-1].touch()
        with mock.patch.object(p.shutil, 'disk_usage', return_value=type('Usage', (), {'free': 2**63})):
            p.provision(replacement, lambda _c, _i, base: base.mkdir(parents=True), execute)
        self.assertEqual(self.reset_counter.call_args_list, [mock.call(1), mock.call(1)])
        self.assertEqual(counters, {1: 0, 2: 987})
        self.assertTrue((p.REGISTRY / 'c.json').is_file())

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

    # Remote compile cache files for the tenant runner. The secrets below are
    # recognizable so a test can show they never appear in an error.
    def cache_dir(self, write_key=True, generation="g1"):
        directory = self.root / "cache-input"
        directory.mkdir(exist_ok=True)
        files = {
            "config.json": {"generation": generation, "tenant": SUB, "endpoint": "https://s3.example",
                            "region": "gra", "bucket": "motor-cache",
                            "prefix": "tenant/" + SUB.replace(":", "-") + "/", "storage_class": "STANDARD",
                            "enabled": True},
            "read-key": {"generation": generation, "access_key_id": "read-id", "secret_access_key": "READ-SECRET"},
        }
        if write_key:
            files["write-key"] = {"generation": generation, "access_key_id": "write-id",
                                  "secret_access_key": "WRITE-SECRET"}
        for name, value in files.items():
            path = directory / name
            path.write_text(json.dumps(value))
            path.chmod(0o444 if name == "config.json" else 0o400)
        return directory

    def provision_with_cache(self, write_key=True):
        """Provision tenant a with cache files; returns what the new disk held
        under state/main/cache-provisioning as {name: (mode, bytes)}, and the
        directory modes on the way there."""
        c = p.checked({**config(), "remote_cache": True})
        files = p.read_remote_cache(self.cache_dir(write_key), c["sub"])
        seen = {}
        def execute(*args):
            if args[0] == "fallocate":
                args[-1].touch()
        def stage(mount):
            directory = mount
            for name, _mode in p.REMOTE_CACHE_DIRECTORIES:
                directory = directory / name
                seen[name] = stat.S_IMODE(directory.stat().st_mode)
            for path in directory.iterdir():
                seen[path.name] = (stat.S_IMODE(path.stat().st_mode), path.read_bytes())
            seen["runner-args"] = (mount / "runner-args").read_text()
        with mock.patch.object(p.shutil, "disk_usage", return_value=type("Usage", (), {"free": 2**63})):
            p.provision(c, lambda _c, _i, path: path.mkdir(parents=True), execute, stage=stage, remote_cache=files)
        (p.PROVISION / "a.json").write_text(json.dumps(c, indent=2) + "\n")
        return c, seen

    def test_cache_files_are_installed_on_the_new_disk_with_the_runner_modes(self):
        for write_key in (True, False):
            with self.subTest(write_key=write_key):
                self.tearDown()
                self.setUp()
                c, seen = self.provision_with_cache(write_key)
                self.assertEqual((seen["state"], seen["main"], seen["cache-provisioning"]), (0o755, 0o700, 0o700))
                self.assertEqual(seen["config.json"][0], 0o444)
                self.assertEqual(seen["read-key"][0], 0o400)
                self.assertIn(b"READ-SECRET", seen["read-key"][1])
                if write_key:
                    self.assertEqual(seen["write-key"][0], 0o400)
                else:
                    self.assertNotIn("write-key", seen)
                self.assertIn(" --remote-cache true ", seen["runner-args"])
                self.assertEqual(seen["runner-args"], p.runner_args(c))
                # The host keeps names and modes only, and the helper registry
                # never sees the provisioning-only switch.
                manifest = (self.root / "tenants/a" / p.REMOTE_CACHE_MANIFEST).read_text()
                self.assertNotIn("SECRET", manifest)
                expected = {"config.json": "0444", "read-key": "0400"}
                if write_key:
                    expected["write-key"] = "0400"
                self.assertEqual(json.loads(manifest), expected)
                self.assertNotIn("remote_cache", json.loads((p.REGISTRY / "a.json").read_text()))

    def test_cache_record_and_files_must_come_together_before_any_mutation(self):
        files = p.read_remote_cache(self.cache_dir(), SUB)
        for c, remote_cache in ((p.checked({**config(), "remote_cache": True}), None), (config(), files)):
            before = self.snapshot_tree()
            with self.assertRaisesRegex(ValueError, "--remote-cache-dir"):
                p.provision(c, mock.Mock(), mock.Mock(), remote_cache=remote_cache)
            self.assertEqual(self.snapshot_tree(), before)
        with self.assertRaisesRegex(ValueError, "remote_cache must be boolean"):
            p.checked({**config(), "remote_cache": "true"})

    def test_runner_args_turn_the_cache_on_only_for_a_cache_record(self):
        plain = p.runner_args(config())
        self.assertNotIn("--remote-cache", plain)
        self.assertEqual(p.runner_args(p.checked({**config(), "remote_cache": False})), plain)
        cached = p.runner_args(p.checked({**config(), "remote_cache": True}))
        self.assertEqual(cached.replace("--remote-cache true ", ""), plain)

    def test_cache_input_errors_name_the_file_but_never_a_secret(self):
        def damaged(change):
            shutil.rmtree(self.root / "cache-input", ignore_errors=True)
            directory = self.cache_dir()
            change(directory)
            return directory
        def rewrite(name, **fields):
            def change(directory):
                path = directory / name
                path.chmod(0o600)
                path.write_text(json.dumps({**json.loads(path.read_text()), **fields}))
                path.chmod(0o444 if name == "config.json" else 0o400)
            return change
        def raw(name, text):
            def change(directory):
                path = directory / name
                path.chmod(0o600)
                path.write_text(text)
                path.chmod(0o400)
            return change
        cases = [
            (lambda d: (d / "read-key").unlink(), "lacks read-key"),
            (lambda d: (d / "extra").write_text("x"), "unexpected file"),
            (rewrite("write-key", generation="g2"), "different generations"),
            (rewrite("config.json", prefix="tenant/" + SUB + "/"), "prefix"),
            (rewrite("config.json", tenant="acct:01ARZ3NDEKTSV4RRFFQ69G5FAW"), "tenant must be"),
            (lambda d: (d / "write-key").chmod(0o440), "owner alone"),
            (raw("write-key", '{"secret_access_key": "WRITE-SECRET" broken'), "not valid JSON"),
            (raw("write-key", '{"secret_access_key": "WRITE-SECRET"}'), "exactly the keys"),
        ]
        for change, message in cases:
            with self.subTest(message=message):
                with self.assertRaises(ValueError) as caught:
                    p.read_remote_cache(damaged(change), SUB)
                self.assertIn(message, str(caught.exception))
                self.assertNotIn("SECRET", str(caught.exception))
        # Without a write key the set is complete for reads.
        shutil.rmtree(self.root / "cache-input")
        self.assertEqual(set(p.read_remote_cache(self.cache_dir(write_key=False), SUB)), {"config.json", "read-key"})

    def test_switch_image_keeps_the_cache_files_and_the_cache_switch(self):
        c, _seen = self.provision_with_cache()
        disk = self.root / "tenants/a/disk.img"
        disk.write_bytes(b"tenant disk holding state/main/cache-provisioning")
        manifest = self.root / "tenants/a" / p.REMOTE_CACHE_MANIFEST
        kept = (disk.read_bytes(), manifest.read_bytes())
        for version in ("v2",):
            image = self.root / "tenant-images" / version
            image.mkdir(parents=True)
            (image / "vmlinux").write_bytes(b"kernel-v2")
            (image / "rootfs").write_bytes(b"root-v2")
            (image / "manifest.json").write_text(json.dumps(dict(
                version=version, kernel_sha256=hashlib.sha256(b"kernel-v2").hexdigest(),
                rootfs_sha256=hashlib.sha256(b"root-v2").hexdigest())))
        p.switch_image("a", "v2", make_base=self.fake_base, check_service=lambda _unit: "inactive",
                       validate_image=mock.Mock(), output=lambda _line: None)
        # switch-image never opens the tenant disk, so the files on it stay.
        self.assertEqual((disk.read_bytes(), manifest.read_bytes()), kept)
        record = p.checked(json.loads((p.PROVISION / "a.json").read_text()))
        self.assertEqual(record["image_version"], "v2")
        self.assertIn(" --remote-cache true ", p.runner_args(record))
        self.assertEqual(hosted.remote_cache_report(record, p).split(";")[0],
                         "remote cache: config.json 0444, read-key 0400, write-key 0400, "
                         "so the tenant fills its own cache")

    def test_check_installed_reports_cache_presence_without_contents(self):
        self.assertEqual(hosted.remote_cache_report(config(), p), "remote cache: off")
        c = p.checked({**config(), "remote_cache": True})
        with self.assertRaisesRegex(ValueError, "cannot be read"):
            hosted.remote_cache_report(c, p)
        c, _seen = self.provision_with_cache(write_key=False)
        report = hosted.remote_cache_report(c, p)
        self.assertIn("no write-key, so the tenant reads the cache and never fills it", report)
        self.assertNotIn("SECRET", report)
        manifest = self.root / "tenants/a" / p.REMOTE_CACHE_MANIFEST
        manifest.write_text(json.dumps({"config.json": "0444", "read-key": "0644"}))
        with self.assertRaisesRegex(ValueError, "runner's modes"):
            hosted.remote_cache_report(c, p)


if __name__ == "__main__":
    unittest.main(verbosity=2)
