#!/usr/bin/env python3
"""Offline resize: preconditions, the resize.json journal, crash recovery
through resume and abort, and the refusals a pending journal imposes."""
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("provision", Path(__file__).with_name("provision.py"))
assert spec is not None and spec.loader is not None
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
hosted_spec = importlib.util.spec_from_file_location(
    "hosted_host_for_resize", Path(__file__).resolve().parents[1] / "hosted-host" / "hosted_host.py")
assert hosted_spec is not None and hosted_spec.loader is not None
hosted = importlib.util.module_from_spec(hosted_spec)
hosted_spec.loader.exec_module(hosted)

MIB = 1048576
GIB = 1024**3
CAPACITY = dict(windows_vm_bytes=16 * GIB, host_reserve_bytes=6 * GIB, jobs_min_bytes=24 * GIB,
                storage_margin_bytes=8 * GIB, growth_allowance_bytes=64 * GIB,
                tenant_memory_reserve_bytes=2 * GIB)
RENDERED = "[Slice]\nMemoryMax=42949672960\nMemoryHigh=35791394134\n"
SUB = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV"
HELPER = {"allowed_uids": [990], "socket_gid": 990, "uid_base": 290000,
          "slots": [{"cpus": "0-1", "mems": "0", "cpu_max": "100000 100000", "memory_max": GIB,
                     "io_weight": 10, "io_max": None}],
          "agent_port": 52, "proxy_port": 53}
# A resize of tenant a touching every field it may change, quota growth included.
NEW = dict(vcpu_ceiling=4, guest_ram_mib=8192, cpuset="0-3", max_weight=16, idle_suspend_s=0,
           disk_quota_bytes=40 * GIB)
# Every hook point of an uninterrupted resize, in order.
FORWARD_POINTS = ["phase:started", "step:base", "phase:base_built", "step:stage:runner_args",
                  "step:stage:copies", "phase:staged", "step:set_aside", "phase:set_aside", "phase:committed",
                  "step:publish:provision", "step:publish:registry", "step:publish:helper", "phase:published",
                  "step:extend", "phase:extended", "step:args"]
ABORT_POINTS = ["phase:aborting", "abort:snapshots", "abort:staged", "abort:base"]
SNAPSHOT = {"key.json": b"{}", "suspend.vmstate": b"old-state", "suspend.memory": b"old-memory"}
VECTOR = Path(__file__).resolve().parents[2] / "crates/motor-vm-helper/tests/vectors/runner-args-staged.json"


class Crash(BaseException):
    """Stops a resize at a hook point the way a killed process would: no
    handler in provision.py catches it."""


class ResizeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        patch = mock.patch.multiple(
            p, ROOT=self.root, REGISTRY=self.root / "registry", PROVISION=self.root / "provision",
            BINDINGS=self.root / "host/tenants.json", JOBS_DROP_IN=self.root / "jobs.conf",
            HELPER_CONFIG=self.root / "motor-vm-helper.json", HELPER_CGROUPS=self.root / "cgroup/tenants",
            read_capacity=lambda: dict(CAPACITY), memtotal_bytes=lambda: 128 * GIB,
            slice_properties=lambda _names: {"LoadState": "loaded", "MemoryMax": "42949672960"},
            runner_processes=lambda: [])
        patch.start()
        self.addCleanup(patch.stop)
        self.free = 1 << 50
        space = mock.patch.object(p.shutil, "disk_usage", side_effect=lambda _path: SimpleNamespace(free=self.free))
        space.start()
        self.addCleanup(space.stop)
        # Every external command a resize could run, to prove it runs none.
        self.commands = []
        for target, name in ((p, "run"), (p.subprocess, "run"), (p.subprocess, "Popen")):
            recorder = mock.patch.object(target, name, side_effect=self.record_command)
            recorder.start()
            self.addCleanup(recorder.stop)
        self.reset()

    def record_command(self, *args, **_kwargs):
        self.commands.append([str(a) for a in (args[0] if len(args) == 1 and isinstance(args[0], list) else args)])
        return SimpleNamespace(stdout="", returncode=0)

    def reset(self):
        """A host with tenant a (2 vCPUs, 4 GiB, 20 GiB) on a resize-v1 image."""
        for child in self.root.iterdir():
            shutil.rmtree(child) if child.is_dir() else child.unlink()
        for directory in (p.REGISTRY, p.PROVISION, p.BINDINGS.parent, self.root / "tenants"):
            directory.mkdir(parents=True)
        p.JOBS_DROP_IN.write_text(RENDERED)
        p.HELPER_CONFIG.write_text(json.dumps(HELPER, indent=2) + "\n")
        self.image = self.root / "tenant-images/v1"
        self.image.mkdir(parents=True)
        (self.image / "vmlinux").write_bytes(b"kernel")
        (self.image / "rootfs").write_bytes(b"root")
        self.manifest(["sparse-v1", "resize-v1"])
        self.bases = []
        self.output = []
        self.config = self.tenant()
        self.fake_base(None, None, p.base_path(p.checked(self.config), self.image))
        self.bases.clear()

    def manifest(self, capabilities):
        (self.image / "manifest.json").write_text(json.dumps(dict(
            version="v1", kernel_sha256=hashlib.sha256(b"kernel").hexdigest(),
            rootfs_sha256=hashlib.sha256(b"root").hexdigest(), capabilities=capabilities)))

    def tenant(self, name="a", tap=1, cpuset="0-1", **extra):
        config = dict(tenant=name, sub=SUB, vm=f"vm-{name}", image_version="v1", vcpu_ceiling=2,
                      guest_ram_mib=4096, disk_quota_bytes=20 * GIB, cpuset=cpuset, tap_index=tap,
                      idle_suspend_s=600, host_floor_bytes=p.RUNNER_DEFAULT_FLOOR, resolvers=["1.1.1.1"])
        config.update(extra)
        (p.PROVISION / f"{name}.json").write_text(json.dumps(config, indent=2) + "\n")
        home = self.root / "tenants" / name
        record = {key: value for key, value in p.checked(config).items() if key not in p.PROVISION_ONLY}
        record.update({key: str(home / leaf) for key, leaf in p.REGISTRY_PATHS.items()})
        (p.REGISTRY / f"{name}.json").write_text(json.dumps(record, indent=2) + "\n")
        (home / "suspend").mkdir(parents=True)
        for leaf, data in SNAPSHOT.items():
            (home / "suspend" / leaf).write_bytes(data)
        with (home / "disk.img").open("wb") as disk:
            disk.truncate(config["disk_quota_bytes"])
        (home / "generation").write_text("3\n")
        return config

    def fake_base(self, _config, _image, base):
        self.bases.append(base)
        base.mkdir(parents=True)
        for name in ("base.vmstate", "base.memory"):
            (base / name).write_bytes(b"base")

    def tree(self):
        # Disks are sparse and many GiB long, so they are compared by length.
        def content(path):
            if not path.is_file():
                return None
            return ("length", path.stat().st_size) if path.name == "disk.img" else path.read_bytes()
        return {str(path.relative_to(self.root)): content(path)
                for path in sorted(self.root.rglob("*")) if path.name != "provision.lock"}

    def resize(self, arguments=None, tenant="a", **kwargs):
        kwargs.setdefault("make_base", self.fake_base)
        kwargs.setdefault("check_service", lambda _unit: "inactive")
        kwargs.setdefault("vmms", lambda: [])
        kwargs.setdefault("output", self.output.append)
        return p.resize(tenant, dict(NEW) if arguments is None else arguments, **kwargs)

    def abort(self, tenant="a", **kwargs):
        kwargs.setdefault("check_service", lambda _unit: "inactive")
        kwargs.setdefault("output", self.output.append)
        return p.resize_abort(tenant, **kwargs)

    def crash_at(self, target, seen=None):
        def hook(point):
            # At every point the journal on disk is complete and well formed.
            if p.resize_journal_path("a").exists():
                p.read_resize_journal("a")
            if seen is not None:
                seen.append(point)
            if point == target:
                raise Crash(point)
        return hook

    def journal_phase(self):
        return p.read_resize_journal("a")["phase"]

    def assert_old_state_intact(self):
        """Records, helper config, disk length and snapshots as before the resize."""
        home = self.root / "tenants/a"
        self.assertEqual(json.loads((p.PROVISION / "a.json").read_text()), self.config)
        self.assertEqual(json.loads((p.REGISTRY / "a.json").read_text())["vcpu_ceiling"], 2)
        self.assertEqual(json.loads(p.HELPER_CONFIG.read_text()), HELPER)
        self.assertEqual((home / "disk.img").stat().st_size, 20 * GIB)
        holders = [home / "suspend", *home.glob("suspend.resize-*")]
        found = [h for h in holders if (h / "suspend.memory").exists()]
        self.assertEqual(len(found), 1)
        self.assertEqual({leaf: (found[0] / leaf).read_bytes() for leaf in SNAPSHOT}, SNAPSHOT)

    def assert_new_state(self):
        home = self.root / "tenants/a"
        new_config = {**self.config, **NEW}
        self.assertEqual((p.PROVISION / "a.json").read_text(), json.dumps(new_config, indent=2) + "\n")
        registry = json.loads((p.REGISTRY / "a.json").read_text())
        for name, value in NEW.items():
            self.assertEqual(registry[name], value)
        self.assertEqual(registry["disk"], str(home / "disk.img"))
        helper = json.loads(p.HELPER_CONFIG.read_text())
        self.assertEqual(helper, {**HELPER, "slots": [{**HELPER["slots"][0], "cpus": "0-3"}]})
        self.assertEqual((home / "disk.img").stat().st_size, 40 * GIB)
        staged = home / "runner-args.staged"
        self.assertEqual(staged.read_text(), p.staged_activation(p.runner_args(p.checked(new_config)), True))
        self.assertIn("--max-weight 16 --rustc-slots 16", staged.read_text())
        self.assertEqual(stat.S_IMODE(staged.stat().st_mode), 0o600)
        self.assertFalse((home / "resize.json").exists())
        self.assertFalse((home / "resize-staged").exists())
        self.assertEqual(list((home / "suspend").iterdir()), [])
        set_aside = list(home.glob("suspend.resize-*"))
        self.assertEqual(len(set_aside), 1)
        self.assertEqual({leaf: (set_aside[0] / leaf).read_bytes() for leaf in SNAPSHOT}, SNAPSHOT)
        self.assertTrue(p.base_present(p.checked(new_config), self.image))
        self.assertEqual(self.commands, [])

    def test_full_resize_publishes_the_new_state_through_every_step(self):
        seen = []
        self.resize(hook=self.crash_at(None, seen))
        self.assertEqual(seen, FORWARD_POINTS)
        self.assert_new_state()
        self.assertEqual(self.bases, [self.image / "base/4-8192"])

    def test_every_precondition_refuses_before_any_mutation_with_its_own_message(self):
        home = self.root / "tenants/a"

        def other_tenant(**extra):
            self.tenant(name="b", tap=2, cpuset="8-9", **extra)

        cases = [
            ("active units", lambda: None, dict(check_service=lambda unit: "active" if unit == "motor-host.service"
                                                else "inactive"), {}, "motor-host.service must be stopped"),
            ("helper active", lambda: None, dict(check_service=lambda unit: "active" if "helper" in unit
                                                 else "inactive"), {}, "motor-vm-helper.service must be stopped"),
            ("live VMM", lambda: None, dict(vmms=lambda: [4242]), {}, "VMM or jailer is still running \\(pid 4242\\)"),
            ("running.json", lambda: (home / "running.json").write_text("{}"), {}, {}, "running.json"),
            ("unclean-stop.json", lambda: (home / "unclean-stop.json").write_text("{}"), {}, {}, "unclean-stop.json"),
            ("leftover cgroup", lambda: (p.HELPER_CGROUPS / "tenant-1").mkdir(parents=True), {}, {},
             "leftover cgroup"),
            ("leftover jail", lambda: (self.root / "jail/tenants/a").mkdir(parents=True), {}, {}, "leftover jail"),
            ("another journal", lambda: (other_tenant(), (self.root / "tenants/b/resize.json").write_text("{}")),
             {}, {}, "another tenant's resize journal"),
            ("pending activation", lambda: (other_tenant(),
                                            (self.root / "tenants/b/runner-args.staged").write_text("{}")),
             {}, {}, "tenant b has a pending resize activation"),
            ("own pending activation", lambda: (home / "runner-args.staged").write_text("{}"), {}, {},
             "tenant a has a pending resize activation"),
            ("no resize-v1", lambda: self.manifest(["sparse-v1"]), {}, {}, "resize-v1"),
            ("disk shrink", lambda: None, {}, dict(disk_quota_bytes=10 * GIB), "disk shrink"),
            ("RAM decrease", lambda: None, {}, dict(guest_ram_mib=2048), "RAM decrease"),
            ("disk length", lambda: (home / "disk.img").write_bytes(b"x"), {}, {}, "disk length 1 differs"),
            ("odd vCPUs", lambda: None, {}, dict(vcpu_ceiling=3, max_weight=16), "1 or an even number"),
            ("too many vCPUs", lambda: None, {}, dict(vcpu_ceiling=34, max_weight=64), "1 or an even number"),
            ("Windows CPUs", lambda: None, {}, dict(cpuset="10-13"), "overlaps the Windows VM's CPUs"),
            ("another cpuset", lambda: other_tenant(), {}, dict(cpuset="6-9"), "overlaps tenant b's cpuset 8-9"),
            ("space", lambda: setattr(self, "free", 100 * GIB), {}, {}, "resize needs [0-9]+ free bytes"),
            ("memory fit", lambda: None, {}, dict(guest_ram_mib=90 * 1024), "memory fit requires"),
            ("drop-in absent", lambda: p.JOBS_DROP_IN.unlink(), {}, {}, "missing or unreadable jobs-slice drop-in"),
            ("drop-in differs", lambda: p.JOBS_DROP_IN.write_text(RENDERED + "\n"), {}, {}, "drop-in differs"),
            ("max_weight above u32", lambda: None, {}, dict(max_weight=2**32), "max_weight must be"),
            ("max_weight below ceiling", lambda: None, {}, dict(max_weight=2), "at least vcpu_ceiling"),
        ]
        for label, prepare, kwargs, change, message in cases:
            with self.subTest(label):
                self.reset()
                self.free = 1 << 50
                prepare()
                before = self.tree()
                with self.assertRaisesRegex(ValueError, message):
                    self.resize({**NEW, **change}, **kwargs)
                self.assertEqual(self.tree(), before)
                self.assertEqual(self.bases, [])
        self.reset()
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "changes nothing"):
            self.resize(dict(vcpu_ceiling=2, cpuset="0-1"))
        self.assertEqual(self.tree(), before)

    def test_windows_cpus_match_the_hosted_host_layout(self):
        self.assertEqual(p.WINDOWS_CPUS, hosted.WINDOWS_CPUS)

    def test_staged_activation_matches_the_helper_vector(self):
        vector = json.loads(VECTOR.read_text())
        self.assertEqual(p.staged_activation(vector["runner_args"], vector["quota_grew"]), VECTOR.read_text())

    def test_journal_write_is_fsynced_renamed_then_directory_synced(self):
        events = []
        real_fsync, real_replace, real_sync = p.os.fsync, p.os.replace, p.sync_dir

        def fsync(fd):
            events.append("fsync")
            real_fsync(fd)

        def replace(source, target):
            events.append(("replace", Path(target).name))
            real_replace(source, target)

        def sync(path):
            events.append(("sync_dir", Path(path)))
            real_sync(path)

        journal = p.plan_resize("a", dict(NEW))
        with mock.patch.object(p.os, "fsync", side_effect=fsync), \
                mock.patch.object(p.os, "replace", side_effect=replace), \
                mock.patch.object(p, "sync_dir", side_effect=sync):
            p.write_resize_journal(journal)
        # The temporary file's fsync, the rename, then the directory's own fsync.
        self.assertEqual(events, ["fsync", ("replace", "resize.json"), ("sync_dir", self.root / "tenants/a"), "fsync"])
        path = p.resize_journal_path("a")
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(p.read_resize_journal("a"), journal)
        self.assertEqual([entry.name for entry in path.parent.iterdir() if "pending" in entry.name], [])

    def test_failed_journal_creation_leaves_the_old_state_and_no_journal(self):
        before = self.tree()
        real_replace = p.os.replace

        def replace(source, target):
            if Path(target).name == "resize.json":
                raise OSError("injected rename failure")
            real_replace(source, target)

        with mock.patch.object(p.os, "replace", side_effect=replace), self.assertRaises(OSError):
            self.resize()
        self.assertEqual(self.tree(), before)

    def test_a_failed_phase_write_leaves_the_previous_phase_and_resume_finishes(self):
        phases = [point.split(":", 1)[1] for point in FORWARD_POINTS if point.startswith("phase:")]
        self.assertEqual(phases, list(p.RESIZE_PHASES))
        for index in range(1, len(phases)):
            with self.subTest(phase=phases[index]):
                self.reset()
                writes = []
                real_replace = p.os.replace

                def replace(source, target, writes=writes, index=index):
                    if Path(target).name == "resize.json":
                        writes.append(target)
                        if len(writes) == index + 1:
                            raise OSError("injected journal rename failure")
                    real_replace(source, target)

                with mock.patch.object(p.os, "replace", side_effect=replace), self.assertRaises(OSError):
                    self.resize()
                self.assertEqual(self.journal_phase(), phases[index - 1])
                if index <= p.RESIZE_COMMITTED:
                    self.assert_old_state_intact()
                self.resize()
                self.assert_new_state()

    def test_a_crash_after_every_step_resumes_to_the_new_state(self):
        for point in FORWARD_POINTS:
            with self.subTest(point=point):
                self.reset()
                with self.assertRaises(Crash):
                    self.resize(hook=self.crash_at(point))
                phase = self.journal_phase()
                if p.RESIZE_PHASES.index(phase) < p.RESIZE_COMMITTED:
                    self.assert_old_state_intact()
                self.resize(hook=self.crash_at(None))
                self.assert_new_state()

    def test_a_crash_before_commit_aborts_to_the_exact_old_state(self):
        for point in FORWARD_POINTS[:FORWARD_POINTS.index("phase:committed")]:
            with self.subTest(point=point):
                self.reset()
                before = self.tree()
                with self.assertRaises(Crash):
                    self.resize(hook=self.crash_at(point))
                self.abort()
                # The base this resize built is gone, the snapshots are back
                # and nothing staged remains: byte for byte the old host.
                self.assertEqual(self.tree(), before)
                self.assertEqual(self.commands, [])

    def test_after_commit_abort_refuses_and_changes_nothing(self):
        for point in FORWARD_POINTS[FORWARD_POINTS.index("phase:committed"):]:
            with self.subTest(point=point):
                self.reset()
                with self.assertRaises(Crash):
                    self.resize(hook=self.crash_at(point))
                before = self.tree()
                with self.assertRaisesRegex(ValueError, "committed and only rolls forward"):
                    self.abort()
                self.assertEqual(self.tree(), before)

    def test_a_crash_during_abort_blocks_resume_and_a_rerun_abort_finishes(self):
        for point in ABORT_POINTS:
            with self.subTest(point=point):
                self.reset()
                before = self.tree()
                with self.assertRaises(Crash):
                    self.resize(hook=self.crash_at("phase:set_aside"))
                seen = []
                with self.assertRaises(Crash):
                    self.abort(hook=self.crash_at(point, seen))
                self.assertEqual(seen, ABORT_POINTS[:ABORT_POINTS.index(point) + 1])
                self.assertEqual(self.journal_phase(), "aborting")
                # The published records stay the old size throughout, so no
                # old snapshot ever pairs with the new sizing.
                self.assert_old_state_intact()
                with self.assertRaisesRegex(ValueError, "being aborted"):
                    self.resize()
                self.abort()
                self.assertEqual(self.tree(), before)

    def test_abort_keeps_a_base_that_another_registered_tenant_uses(self):
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("phase:base_built"))
        self.assertEqual(self.bases, [self.image / "base/4-8192"])
        # A registry entry naming the new size's base, however it appeared.
        self.tenant(name="b", tap=2, cpuset="8-11", vcpu_ceiling=4, guest_ram_mib=8192)
        self.abort()
        self.assertTrue(p.base_present(dict(vcpu_ceiling=4, guest_ram_mib=8192), self.image))
        self.assertFalse(p.resize_journal_path("a").exists())

    def test_abort_keeps_a_base_that_existed_before_the_resize(self):
        self.fake_base(None, None, self.image / "base/4-8192")
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("phase:set_aside"))
        self.assertTrue(p.read_resize_journal("a")["base_existed"])
        self.abort()
        self.assertTrue((self.image / "base/4-8192/base.memory").exists())

    def test_resume_with_other_arguments_or_an_unexpected_disk_length_refuses(self):
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("phase:staged"))
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "differ from the pending resize"):
            self.resize({**NEW, "max_weight": 32})
        self.assertEqual(self.tree(), before)
        disk = self.root / "tenants/a/disk.img"
        with disk.open("r+b") as file:
            file.truncate(30 * GIB)
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "disk length 32212254720 is not the expected 21474836480 bytes"):
            self.resize()
        self.assertEqual(self.tree(), before)
        # After commit both the old and the new length are expected; no other.
        self.reset()
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("step:extend"))
        with disk.open("r+b") as file:
            file.truncate(30 * GIB)
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "not the expected 21474836480 or 42949672960 bytes"):
            self.resize()
        self.assertEqual(self.tree(), before)
        with disk.open("r+b") as file:
            file.truncate(20 * GIB)
        self.resize()
        self.assert_new_state()

    def test_malformed_journal_refuses_resize_and_abort(self):
        valid = p.plan_resize("a", dict(NEW))
        variants = {
            "truncated": json.dumps(valid)[:40],
            "not an object": "[]",
            "unknown version": json.dumps({**valid, "schema": 2}),
            "missing key": json.dumps({k: v for k, v in valid.items() if k != "set_aside"}),
            "unknown phase": json.dumps({**valid, "phase": "half-done"}),
            "foreign base": json.dumps({**valid, "base": "/etc"}),
            "foreign set-aside": json.dumps({**valid, "set_aside": str(self.root / "registry")}),
            "foreign staged": json.dumps({**valid, "staged": {**valid["staged"], "runner_args": "/etc/passwd"}}),
        }
        for label, content in variants.items():
            with self.subTest(label):
                p.resize_journal_path("a").write_text(content)
                before = self.tree()
                with self.assertRaisesRegex(ValueError, "malformed resize journal"):
                    self.resize()
                with self.assertRaisesRegex(ValueError, "malformed resize journal"):
                    self.abort()
                self.assertEqual(self.tree(), before)

    def test_a_pending_journal_refuses_every_other_sizing_change(self):
        self.tenant(name="b", tap=2, cpuset="8-9")
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("phase:set_aside"))
        before = self.tree()
        new_tenant = p.checked(dict(tenant="c", sub=SUB, vm="vm-c", image_version="v1", vcpu_ceiling=2,
                                    guest_ram_mib=4096, disk_quota_bytes=20 * GIB, cpuset="10-11", tap_index=3,
                                    resolvers=["1.1.1.1"]))
        refusals = [
            ("provision", lambda: p.provision(new_tenant, make_base=self.fake_base, execute=self.record_command),
             "pending resize journal"),
            ("resize b", lambda: self.resize(dict(max_weight=8), tenant="b"), "another tenant's resize journal"),
            ("switch-image", lambda: p.switch_image("b", "v1", make_base=self.fake_base,
                                                    check_service=lambda _unit: "inactive",
                                                    validate_image=lambda _image: None, output=self.output.append),
             "pending resize journal"),
            ("repair-from-registry", lambda: p.repair_from_registry("b", validate_image=lambda _image: None,
                                                                    check_trusted=lambda _path: None,
                                                                    output=self.output.append),
             "pending resize journal"),
            ("apply-jobs-slice", p.apply_jobs_slice, "pending resize journal"),
        ]
        for label, action, message in refusals:
            with self.subTest(label):
                with self.assertRaisesRegex(ValueError, message):
                    action()
                self.assertEqual(self.tree(), before)
        self.assertEqual(self.commands, [])
        # remove-tenant still runs, and so do the journaled tenant's abort and resume.
        p.remove_tenant("b", purge_now=True, check_service=lambda _unit: "inactive", execute=self.record_command,
                        has_tap=lambda _tap: False, now=lambda: 1000, output=self.output.append)
        self.assertFalse((p.REGISTRY / "b.json").exists())
        self.abort()
        self.assertFalse(p.resize_journal_path("a").exists())
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("phase:set_aside"))
        self.resize()
        self.assert_new_state()

    def test_a_committed_activation_blocks_the_next_resize_and_stays_staged(self):
        self.tenant(name="b", tap=2, cpuset="8-9")
        self.resize()
        staged = (self.root / "tenants/a/runner-args.staged").read_bytes()
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "tenant a has a pending resize activation"):
            self.resize(dict(max_weight=8), tenant="b")
        self.assertEqual(self.tree(), before)
        self.assertEqual((self.root / "tenants/a/runner-args.staged").read_bytes(), staged)

    def test_a_max_weight_only_resize_needs_the_full_pre_operation_space(self):
        # Tenant a idles after 600 s, so its suspend reserve (4 GiB) counts:
        # threshold 128 + 4 + 8 GiB, requirement + 8 + 64 GiB, + 2 GiB of mkfs
        # metadata. The size base already exists, so nothing more.
        exact = (128 + 4 + 8 + 8 + 64 + 2) * GIB
        self.free = 160 * GIB
        before = self.tree()
        with self.assertRaisesRegex(ValueError, f"resize needs {exact} free bytes.*only max_weight or idle_suspend_s"):
            self.resize(dict(max_weight=8))
        self.assertEqual(self.tree(), before)
        self.free = exact - 1
        with self.assertRaisesRegex(ValueError, f"resize needs {exact} free bytes"):
            self.resize(dict(max_weight=8))
        self.free = exact
        self.resize(dict(max_weight=8))
        self.assertEqual(json.loads((p.REGISTRY / "a.json").read_text())["max_weight"], 8)
        self.assertIn("--max-weight 8 --rustc-slots 8",
                      json.loads((self.root / "tenants/a/runner-args.staged").read_text())["runner_args"])
        self.assertFalse(json.loads((self.root / "tenants/a/runner-args.staged").read_text())["quota_grew"])

    def test_resize_space_counts_the_larger_term_of_every_tenant_and_an_absent_base(self):
        # Tenant b's 256 GiB host floor sets the host-wide threshold; the new
        # 8 GiB base is absent, so its RAM and the 512 MiB placeholder count.
        self.tenant(name="b", tap=2, cpuset="8-9", host_floor_bytes=256 * GIB, idle_suspend_s=0)
        exact = (256 + 8 + 8 + 64 + 2 + 8) * GIB + 512 * MIB
        self.free = exact - 1
        with self.assertRaisesRegex(ValueError, f"resize needs {exact} free bytes"):
            self.resize()
        self.free = exact
        self.resize()
        self.assert_new_state()

    def test_idle_zero_and_an_unchanged_cpuset_keep_the_helper_config_bytes(self):
        p.HELPER_CONFIG.write_text(json.dumps(HELPER))
        helper = p.HELPER_CONFIG.read_bytes()
        self.resize(dict(idle_suspend_s=0, disk_quota_bytes=30 * GIB))
        self.assertEqual(p.HELPER_CONFIG.read_bytes(), helper)
        self.assertEqual(json.loads((p.REGISTRY / "a.json").read_text())["idle_suspend_s"], 0)
        self.assertEqual((self.root / "tenants/a/disk.img").stat().st_size, 30 * GIB)

    def test_resize_never_mounts_the_tenant_disk(self):
        # Through a crash, a resume and an abort, no external command runs at
        # all, so mount never touches the tenant filesystem.
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("phase:staged"))
        self.abort()
        with self.assertRaises(Crash):
            self.resize(hook=self.crash_at("step:publish:registry"))
        self.resize()
        self.assertEqual([command for command in self.commands if any("mount" in word for word in command)], [])
        self.assertEqual(self.commands, [])


if __name__ == "__main__":
    unittest.main()
