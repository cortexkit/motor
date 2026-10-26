#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import stat
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("provision", Path(__file__).with_name("provision.py"))
assert spec is not None and spec.loader is not None
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)

MIB = 1048576
GIB = 1024**3
CAPACITY = dict(windows_vm_bytes=16 * GIB, host_reserve_bytes=6 * GIB, jobs_min_bytes=24 * GIB,
                storage_margin_bytes=8 * GIB, growth_allowance_bytes=64 * GIB,
                tenant_memory_reserve_bytes=2 * GIB)
RENDERED = "[Slice]\nMemoryMax=42949672960\nMemoryHigh=35791394134\n"


class CapacityTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        patch = mock.patch.multiple(p, ROOT=self.root, REGISTRY=self.root / "registry",
                                    BINDINGS=self.root / "bindings.json",
                                    CAPACITY=self.root / "capacity.json", MEMINFO=self.root / "meminfo",
                                    JOBS_DROP_IN=self.root / "systemd/motor-jobs.slice.d/50-capacity.conf")
        patch.start()
        self.addCleanup(patch.stop)
        p.REGISTRY.mkdir()
        (self.root / "tenants").mkdir()
        p.CAPACITY.write_text(json.dumps(CAPACITY))
        p.MEMINFO.write_text("MemTotal:       131491984 kB\nMemAvailable: 1 kB\n")
        self.metadata = SimpleNamespace(st_uid=0, st_mode=stat.S_IFREG | 0o644)
        owner = mock.patch.object(p.os, "fstat", side_effect=lambda _fd: self.metadata)
        owner.start()
        self.addCleanup(owner.stop)
        self.current = 0
        self.maximum = 128 * GIB * 85 // 100
        self.load_state = "loaded"
        self.active_state = "active"
        self.control_group = "/motor-jobs.slice"
        self.fail_reload = False
        self.apply_limit = True
        self.events = []
        systemd = mock.patch.object(p, "run", side_effect=self.systemctl)
        systemd.start()
        self.addCleanup(systemd.stop)

    def systemctl(self, *args):
        self.events.append(args)
        if args == ("systemctl", "daemon-reload"):
            self.assertEqual(p.JOBS_DROP_IN.read_text(), RENDERED)
            if self.fail_reload:
                raise subprocess.CalledProcessError(1, args, stderr="injected reload failure")
            if self.apply_limit:
                self.maximum = 40 * GIB
            return SimpleNamespace(stdout="")
        self.assertEqual(args[:3], ("systemctl", "show", "-p"))
        self.assertEqual(args[4:], ("motor-jobs.slice",))
        values = {"LoadState": self.load_state, "MemoryCurrent": str(self.current), "MemoryMax": str(self.maximum),
                  "ActiveState": self.active_state, "ControlGroup": self.control_group}
        return SimpleNamespace(stdout="".join(f"{name}={values[name]}\n" for name in args[3].split(",")))

    def tenant(self, name="a", ram_mib=79872, headroom_mib=512):
        record = dict(guest_ram_mib=ram_mib, memory_headroom_mib=headroom_mib)
        (p.REGISTRY / f"{name}.json").write_text(json.dumps(record))
        return dict(record, tenant=name)

    def tree(self):
        return {str(path.relative_to(self.root)): path.read_bytes() if path.is_file() else None
                for path in self.root.rglob("*") if path.name != "provision.lock"}

    def assert_capacity_refused_without_writes(self):
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "capacity.json"):
            p.provision(dict(tenant="new", guest_ram_mib=1024, memory_headroom_mib=512))
        self.assertEqual(self.tree(), before)
        with self.assertRaisesRegex(ValueError, "capacity.json"):
            p.apply_jobs_slice()
        self.assertEqual(self.tree(), before)
        self.assertFalse((self.root / "provision.lock").exists())
        self.assertEqual(self.events, [])

    def test_capacity_schema_refusals_write_nothing(self):
        invalid = ["{", "[]", '{"jobs_min_bytes": 1, "jobs_min_bytes": 2}']
        for key in CAPACITY:
            invalid.append(json.dumps({name: value for name, value in CAPACITY.items() if name != key}))
            for value in (True, "1", 1.0, None, -1, 2**64):
                invalid.append(json.dumps(dict(CAPACITY, **{key: value})))
        invalid.extend(json.dumps(value) for value in (dict(CAPACITY, surprise=0), dict(CAPACITY, jobs_min_bytes=0),
                                                      dict(CAPACITY, windows_vm_bytes=2**64 - 1)))
        for content in invalid:
            with self.subTest(content=content):
                p.CAPACITY.write_text(content)
                self.assert_capacity_refused_without_writes()

    def test_missing_unreadable_and_untrusted_capacity_write_nothing(self):
        p.CAPACITY.unlink()
        self.assert_capacity_refused_without_writes()
        p.CAPACITY.write_text(json.dumps(CAPACITY))
        with mock.patch.object(p.os, "open", side_effect=PermissionError("unreadable")):
            self.assert_capacity_refused_without_writes()
        for uid, mode in ((1, 0o644), (0, 0o664), (0, 0o646)):
            with self.subTest(uid=uid, mode=mode):
                self.metadata = SimpleNamespace(st_uid=uid, st_mode=stat.S_IFREG | mode)
                self.assert_capacity_refused_without_writes()

    def test_capacity_valid_bounds_and_exact_keys(self):
        self.assertEqual(p.read_capacity(), CAPACITY)
        minimal = dict.fromkeys(CAPACITY, 0)
        minimal["jobs_min_bytes"] = 1
        p.CAPACITY.write_text(json.dumps(minimal))
        self.assertEqual(p.read_capacity(), minimal)
        minimal["growth_allowance_bytes"] = 2**64 - 1
        p.CAPACITY.write_text(json.dumps(minimal))
        self.assertEqual(p.read_capacity(), minimal)
        minimal["windows_vm_bytes"] = 2
        p.CAPACITY.write_text(json.dumps(minimal))
        self.assertEqual(p.read_capacity(), minimal)
        self.assertEqual(p.jobs_slice_render(minimal), "[Slice]\nMemoryMax=3\nMemoryHigh=3\n")
        minimal["windows_vm_bytes"] = 2**64 - 2
        p.CAPACITY.write_text(json.dumps(minimal))
        self.assertEqual(p.read_capacity(), minimal)

    def test_jobs_slice_sum_overflow_refuses_without_writes(self):
        capacity = dict(CAPACITY, jobs_min_bytes=1, windows_vm_bytes=2**64 - 1)
        p.CAPACITY.write_text(json.dumps(capacity))
        self.assert_capacity_refused_without_writes()
        with self.assertRaisesRegex(ValueError, "jobs-slice MemoryMax"):
            p.jobs_slice_render(capacity)

    def test_cli_invalid_capacity_precedes_directory_and_lock_creation(self):
        config = self.root / "new.json"
        config.write_text(json.dumps(dict(tenant="new", vm="new", sub="acct:01ARZ3NDEKTSV4RRFFQ69G5FAV",
                                         image_version="v1", vcpu_ceiling=2, cpuset="0-1", tap_index=1,
                                         guest_ram_mib=79872, disk_quota_bytes=20 * GIB, resolvers=["1.1.1.1"])))
        p.CAPACITY.unlink()
        before = self.tree()
        for argv in (["provision.py", "--config", str(config)], ["provision.py", "apply-jobs-slice"]):
            with self.subTest(argv=argv), mock.patch.object(p, "ROOT", self.root / "uncreated"), \
                    mock.patch.object(p, "trusted"), mock.patch.object(p.os, "geteuid", return_value=0), \
                    mock.patch.object(p.sys, "argv", argv), self.assertRaisesRegex(ValueError, "capacity.json"):
                p.main()
            self.assertEqual(self.tree(), before)

    def test_provision_fit_and_dropin_preflights_write_nothing(self):
        proposed = dict(tenant="new", vm="new", image_version="v1", tap_index=1, vcpu_ceiling=2,
                        guest_ram_mib=81920, memory_headroom_mib=512)
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "memory fit requires"):
            p.provision(proposed)
        self.assertEqual(self.tree(), before)
        proposed["guest_ram_mib"] = 79872
        with self.assertRaisesRegex(ValueError, "missing or unreadable jobs-slice drop-in"):
            p.provision(proposed)
        self.assertEqual(self.tree(), before)
        p.JOBS_DROP_IN.parent.mkdir(parents=True)
        for rendered, message in ((RENDERED + "\n", "drop-in differs"), (RENDERED, "MemoryMax=.* exceeds")):
            p.JOBS_DROP_IN.write_text(rendered)
            before = self.tree()
            with self.assertRaisesRegex(ValueError, message):
                p.provision(proposed)
            self.assertEqual(self.tree(), before)

    def test_memory_fit_exact_limit_includes_headroom(self):
        p.MEMINFO.write_text(f"MemTotal: {128 * GIB // 1024} kB\n")
        tenant = self.tenant(ram_mib=83456)
        p.memory_fit(CAPACITY)
        tenant["guest_ram_mib"] = 83457
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "memory fit requires 137440002048 bytes"):
            p.memory_fit(CAPACITY, replacement=tenant)
        self.assertEqual(self.tree(), before)

    def test_memory_fit_measured_host_and_two_tenants(self):
        self.tenant()
        p.memory_fit(CAPACITY)
        self.tenant(name="b", ram_mib=1024)
        with self.assertRaisesRegex(ValueError, "memory fit requires 135291469824 bytes"):
            p.memory_fit(CAPACITY)

    def test_memory_fit_checked_arithmetic_and_memtotal(self):
        self.tenant(ram_mib=2**64 - 1)
        with self.assertRaisesRegex(ValueError, "memory fit sum"):
            p.memory_fit(CAPACITY)
        self.tenant()
        for text in ("MemTotal: 18014398509481984 kB\n", "MemTotal: infinity kB\n", "MemAvailable: 1 kB\n"):
            with self.subTest(text=text):
                p.MEMINFO.write_text(text)
                with self.assertRaisesRegex(ValueError, "MemTotal"):
                    p.memory_fit(CAPACITY)
        with mock.patch.object(Path, "read_text", side_effect=PermissionError("unreadable MemTotal")):
            with self.assertRaisesRegex(ValueError, "MemTotal"):
                p.memory_fit(CAPACITY, replacement=dict(tenant="a", guest_ram_mib=79872, memory_headroom_mib=512))

    def test_jobs_slice_render_adds_windows_to_direct_jobs_budget(self):
        self.assertEqual(p.jobs_slice_render(CAPACITY), RENDERED)
        for maximum, high in ((6, 5), (5, 5), (1, 1)):
            with self.subTest(maximum=maximum):
                self.assertEqual(p.jobs_slice_render(dict(CAPACITY, jobs_min_bytes=maximum, windows_vm_bytes=0)),
                                 f"[Slice]\nMemoryMax={maximum}\nMemoryHigh={high}\n")

    def test_apply_writes_then_reloads_then_reads_back(self):
        original = p.atomic
        def write(path, value):
            self.events.append(("write",))
            original(path, value)
        with mock.patch.object(p, "atomic", side_effect=write):
            p.apply_jobs_slice()
        self.assertEqual(p.JOBS_DROP_IN.read_text(), RENDERED)
        self.assertEqual(stat.S_IMODE(p.JOBS_DROP_IN.stat().st_mode), 0o644)
        self.assertEqual(self.maximum, 42949672960)
        self.assertEqual(self.events, [("systemctl", "show", "-p", "LoadState,MemoryCurrent,ActiveState,ControlGroup", "motor-jobs.slice"),
                                       ("write",), ("systemctl", "daemon-reload"),
                                       ("systemctl", "show", "-p", "LoadState,MemoryMax", "motor-jobs.slice")])

    def test_apply_refuses_above_cap_readback_and_reload_failure(self):
        self.apply_limit = False
        with self.assertRaisesRegex(ValueError, "MemoryMax=.* exceeds 42949672960"):
            p.apply_jobs_slice()
        self.fail_reload = True
        with self.assertRaisesRegex(ValueError, "failed to apply jobs-slice"):
            p.apply_jobs_slice()

    def test_apply_writes_dropin_even_without_installed_slice(self):
        self.load_state = "not-found"
        self.current = "[not set]"
        p.apply_jobs_slice()
        self.assertEqual(p.JOBS_DROP_IN.read_text(), RENDERED)
        self.assertIn(("systemctl", "daemon-reload"), self.events)

    def test_apply_accepts_an_implicit_inactive_slice_without_a_cgroup(self):
        self.current = "[not set]"
        self.active_state = "inactive"
        self.control_group = ""
        p.apply_jobs_slice()
        self.assertEqual(p.JOBS_DROP_IN.read_text(), RENDERED)
        self.assertEqual(self.maximum, 42949672960)

    def test_apply_refuses_unset_usage_if_the_slice_can_have_processes(self):
        self.current = "[not set]"
        for state, group in [("active", ""), ("inactive", "/motor-jobs.slice"), ("failed", "")]:
            with self.subTest(state=state, group=group):
                self.active_state, self.control_group = state, group
                before = self.tree()
                with self.assertRaisesRegex(ValueError, "MemoryCurrent is not a byte value"):
                    p.apply_jobs_slice()
                self.assertEqual(self.tree(), before)

    def test_apply_fit_breaking_proposal_preserves_previous_dropin_and_live_cap(self):
        self.tenant()
        p.apply_jobs_slice()
        p.CAPACITY.write_text(json.dumps(dict(CAPACITY, jobs_min_bytes=64 * GIB)))
        before = self.tree()
        self.events.clear()
        with self.assertRaisesRegex(ValueError, "memory fit requires"):
            p.apply_jobs_slice()
        self.assertEqual(self.tree(), before)
        self.assertEqual(self.maximum, 42949672960)
        self.assertEqual(self.events, [])

    def test_apply_usage_equal_to_high_passes_one_byte_above_writes_nothing(self):
        self.current = 35791394134
        p.apply_jobs_slice()
        self.current = 35791394135
        before = self.tree()
        self.events.clear()
        with self.assertRaisesRegex(ValueError, "MemoryCurrent=35791394135 exceeds rendered MemoryHigh=35791394134"):
            p.apply_jobs_slice()
        self.assertEqual(self.tree(), before)
        self.assertEqual(self.maximum, 42949672960)
        self.assertEqual(len(self.events), 1)

    def test_apply_refuses_any_resize_journal_without_writes(self):
        home = self.root / "tenants/other"
        home.mkdir()
        (home / "resize.json").write_text("malformed journal")
        before = self.tree()
        with self.assertRaisesRegex(ValueError, "pending resize journal"):
            p.apply_jobs_slice()
        self.assertEqual(self.tree(), before)
        self.assertEqual(self.events, [])

    def test_storage_requirement_pinned_to_the_cutover_figures(self):
        # A 78 GiB tenant with idle off and a 600 GiB disk: the suspend reserve
        # is not counted, so the threshold is the 128 GiB host floor plus the
        # 8 GiB storage margin, the requirement adds the margin again and the
        # 64 GiB growth allowance, and the pre-operation figure adds 2 GiB of
        # mkfs metadata and the absent base's RAM plus placeholder.
        tenant = dict(tenant="tenant-1", guest_ram_mib=79872, idle_suspend_s=0,
                      host_floor_bytes=128 * GIB, disk_quota_bytes=600 * GIB)
        self.assertEqual(p.storage_threshold(tenant, CAPACITY), 136 * GIB)
        self.assertEqual(p.operating_requirement(tenant, CAPACITY), 223875170304 - 512 * MIB)
        self.assertEqual(p.pre_operation_free_bytes(tenant, CAPACITY, True), 225485783040)
        self.assertEqual(p.pre_operation_free_bytes(tenant, CAPACITY, False),
                         225485783040 + 79872 * MIB + 512 * MIB)

    def test_storage_requirement_counts_idle_reserve_and_the_larger_tenant(self):
        tenant = dict(tenant="tenant-1", guest_ram_mib=79872, idle_suspend_s=0,
                      host_floor_bytes=128 * GIB, disk_quota_bytes=600 * GIB)
        idle = dict(tenant, idle_suspend_s=600)
        # Idle suspend reserves the guest's RAM in the threshold itself, and
        # the requirement carries it once more through the host floor term.
        self.assertEqual(p.storage_threshold(idle, CAPACITY), 136 * GIB + 79872 * MIB)
        self.assertEqual(p.pre_operation_free_bytes(idle, CAPACITY, True),
                         p.pre_operation_free_bytes(tenant, CAPACITY, True) + 79872 * MIB)
        # A second tenant with a larger host floor sets a higher threshold.
        big = dict(tenant="tenant-2", guest_ram_mib=79872, idle_suspend_s=0,
                   host_floor_bytes=256 * GIB, disk_quota_bytes=600 * GIB)
        self.assertGreater(p.storage_threshold(big, CAPACITY), p.storage_threshold(tenant, CAPACITY))

    def test_host_floor_below_the_direct_runner_floor_is_refused(self):
        with self.assertRaisesRegex(ValueError, "at least the direct runner floor"):
            p.checked(dict(tenant="a", sub="acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", vm="vm-a", image_version="v1",
                           vcpu_ceiling=2, guest_ram_mib=18432, disk_quota_bytes=20 * GIB, cpuset="0-1",
                           tap_index=1, host_floor_bytes=p.RUNNER_DEFAULT_FLOOR - 1, resolvers=["1.1.1.1"]))

    def test_live_cap_exact_limit_and_not_found_require_exact_dropin(self):
        p.apply_jobs_slice()
        p.verify_jobs_slice(CAPACITY)
        self.maximum = 42949672961
        with self.assertRaisesRegex(ValueError, "MemoryMax=42949672961 exceeds"):
            p.verify_jobs_slice(CAPACITY)
        self.maximum = "infinity"
        with self.assertRaisesRegex(ValueError, "MemoryMax is not a byte value: infinity"):
            p.verify_jobs_slice(CAPACITY)
        self.load_state = "not-found"
        p.verify_jobs_slice(CAPACITY)
        p.JOBS_DROP_IN.write_text(RENDERED + "\n")
        with self.assertRaisesRegex(ValueError, "drop-in differs"):
            p.verify_jobs_slice(CAPACITY)
        p.JOBS_DROP_IN.unlink()
        with self.assertRaisesRegex(ValueError, "missing or unreadable jobs-slice drop-in"):
            p.verify_jobs_slice(CAPACITY)


if __name__ == "__main__":
    unittest.main()
