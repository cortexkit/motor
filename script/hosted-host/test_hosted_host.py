"""Run with python3 -m unittest -v script/hosted-host/test_hosted_host.py."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import re
import socket
import struct
import subprocess
import tempfile
import threading
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
spec = importlib.util.spec_from_file_location("hosted_host", HERE / "hosted_host.py")
if spec is None or spec.loader is None:
    raise ImportError("cannot load hosted_host.py")
hosted = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hosted)


def struct_fields(path, name):
    """Field names of a serde struct, read from the Rust source that parses it."""
    source = (REPO / path).read_text()
    body = re.search(r"(?:pub )?struct " + name + r" \{(.*?)\n\}", source, re.S)
    assert body is not None, name
    return {m[1] for m in re.finditer(r"^\s*(?:pub )?(\w+):", body[1], re.M)}


# Placeholders: the real host id and broker subject are install arguments, never
# committed (this repository is public).
HOST_ID = "test-host"
SUB = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV"


def memory_max_mib(vcpus):
    # motor_lease::memory_max_mib: the per-job kill cap a full-size lease grants.
    return max(vcpus * 3584, 16384)


# capacity.json tenant_memory_reserve_bytes (2147483648): a tenant advertises
# its guest's available memory minus this to the broker as free_mem_mib.
TENANT_MEMORY_RESERVE_MIB = 2147483648 // 1048576


def advertised_mib(guest_ram_mib, reserve_mib=TENANT_MEMORY_RESERVE_MIB):
    """The most free_mem_mib the tenant ever advertises: all of its RAM, less
    the reserve (what a stopped tenant advertises)."""
    return guest_ram_mib - reserve_mib


def placeable_vcpus(guest_ram_mib, reserve_mib=TENANT_MEMORY_RESERVE_MIB):
    """The largest lease, in vCPUs, the broker can place on the tenant: the
    broker needs memory_max(vcpus) MiB of free_mem_mib. CLOUD's max_job_vcpus
    ceiling for the host is recorded at this N."""
    n = 0
    while memory_max_mib(n + 1) <= advertised_mib(guest_ram_mib, reserve_mib):
        n += 1
    return n


def cutover_rule(guest_ram_mib, placeable, reserve_mib=TENANT_MEMORY_RESERVE_MIB):
    """docs/hosted-cutover.md's sizing rule: the guest advertises at least
    memory_max of the placeable N recorded with CLOUD's ceilings."""
    return advertised_mib(guest_ram_mib, reserve_mib) >= memory_max_mib(placeable)


def renderer_cli(*args):
    """Run the renderer's parser with the test host's capacity loader."""
    stdout, stderr = io.StringIO(), io.StringIO()
    with mock.patch.object(hosted.sys, "argv", ["hosted_host.py", *map(str, args)]), \
            contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        try:
            code = hosted.main()
        except SystemExit as error:
            assert isinstance(error.code, int)
            code = error.code
    return subprocess.CompletedProcess(args, code, stdout.getvalue(), stderr.getvalue())


class CapacityConfigTests(unittest.TestCase):
    def setUp(self):
        self.capacity = dict(CAPACITY)
        for patch in (mock.patch.object(hosted, "load_provision", return_value=provision),
                      mock.patch.object(provision, "read_capacity", side_effect=lambda: dict(self.capacity))):
            patch.start()
            self.addCleanup(patch.stop)


class HostedHostConfigTests(CapacityConfigTests):
    def test_helper_uplink_is_explicit_and_rendered_for_a_different_host(self):
        config = hosted.helper_config(998, 998, 'eno1', '8.8.8.8')
        self.assertEqual((config['uplink'], config['public_ipv4']), ('eno1', '8.8.8.8'))
        for uplink in ['', 'lo', 'mtap*', 'eno1";accept']:
            with self.subTest(uplink=uplink), self.assertRaises(ValueError):
                hosted.helper_config(998, 998, uplink, '8.8.8.8')

    def test_host_config_has_every_required_motor_host_field_and_no_fake_broker(self):
        config = hosted.host_config(HOST_ID)
        fields = struct_fields("crates/motor-host/src/main.rs", "Config")
        self.assertEqual(set(config), fields - {"fake_broker_url"})
        self.assertEqual(config["host_id"], HOST_ID)
        self.assertEqual(config["listen"], "0.0.0.0:8443")
        self.assertEqual(config["resolvers"], ["1.1.1.1", "9.9.9.9"])
        self.assertEqual(config["storage"], "/var/lib/motor-vm")
        self.assertEqual(hosted.host_config(HOST_ID, port=9443)["listen"], "0.0.0.0:9443")
        for port in (22, 443, 1024, 65536):
            with self.subTest(port=port), self.assertRaises(ValueError):
                hosted.host_config(HOST_ID, port=port)
        for host_id in ("", "host 1", "a" * 65):
            with self.subTest(host_id=host_id), self.assertRaises(ValueError):
                hosted.host_config(host_id)

    def test_helper_config_matches_the_helper_struct_and_admits_only_the_host_account(self):
        config = hosted.helper_config(998, 998, "enp6s0", "8.8.8.8")
        self.assertEqual(set(config), struct_fields("crates/motor-vm-helper/src/config.rs", "Config") - {"proxy_for_jobs"})
        self.assertEqual(set(config["slots"][0]), struct_fields("crates/motor-vm-helper/src/config.rs", "SlotConfig"))
        self.assertEqual(config["allowed_uids"], [998])
        self.assertEqual(config["socket_gid"], 998)
        self.assertEqual(config["firecracker"], "/usr/local/lib/firecracker/1.17.0/firecracker")
        self.assertEqual(config["jailer"], "/usr/local/lib/firecracker/1.17.0/jailer")
        self.assertEqual(config["loader"], "/usr/local/libexec/motor-vm-loader")
        fixture = json.loads((REPO / "crates/motor-vm-helper/fixtures/helper.json").read_text())
        self.assertEqual(config["loader"], fixture["loader"])
        for slot in config["slots"]:
            self.assertFalse(hosted.cpus(slot["cpus"]) & hosted.cpus(hosted.WINDOWS_CPUS))
        with self.assertRaises(ValueError):
            hosted.helper_config(0, 0, "enp6s0", "8.8.8.8")

    def test_helper_accepts_windows_threads_without_a_windows_vm(self):
        self.capacity["windows_vm_bytes"] = 0
        for cpuset in ("12-15", "28-31"):
            with self.subTest(cpuset=cpuset):
                helper = hosted.helper_config(998, 998, "enp6s0", "8.8.8.8", {"cpuset": cpuset})
                self.assertEqual(helper["slots"][0]["cpus"], cpuset)

    def test_validate_tenant_accepts_windows_threads_without_a_windows_vm(self):
        self.capacity["windows_vm_bytes"] = 0
        template = dict(json.loads(hosted.TENANT_FILE.read_text()), sub=SUB, vcpu_ceiling=4)
        for cpuset in ("12-15", "28-31"):
            with self.subTest(cpuset=cpuset):
                self.assertIsNone(hosted.validate_tenant(dict(template, cpuset=cpuset)))

    def test_helper_still_parses_the_cpuset_without_a_windows_vm(self):
        self.capacity["windows_vm_bytes"] = 0
        with self.assertRaises(ValueError):
            hosted.helper_config(998, 998, "enp6s0", "8.8.8.8", {"cpuset": "not-a-cpuset"})

    def test_helpers_and_tenants_reserve_threads_for_any_positive_windows_budget(self):
        self.capacity["windows_vm_bytes"] = 1
        template = dict(json.loads(hosted.TENANT_FILE.read_text()), sub=SUB, vcpu_ceiling=4)
        for cpuset in ("12-15", "28-31"):
            with self.subTest(cpuset=cpuset):
                record = dict(template, cpuset=cpuset)
                with self.assertRaisesRegex(ValueError, "overlaps the Windows VM's CPUs"):
                    hosted.helper_config(998, 998, "enp6s0", "8.8.8.8", record)
                with self.assertRaisesRegex(ValueError, "overlaps the Windows VM's CPUs"):
                    hosted.validate_tenant(record)

    def test_capacity_load_failures_refuse_helpers_and_tenants(self):
        record = dict(json.loads(hosted.TENANT_FILE.read_text()), sub=SUB)
        for error in ("invalid capacity.json: missing", "capacity.json windows_vm_bytes must be a u64"):
            with self.subTest(error=error), mock.patch.object(provision, "read_capacity", side_effect=ValueError(error)) as read:
                with self.assertRaisesRegex(ValueError, "capacity.json"):
                    hosted.helper_config(998, 998, "enp6s0", "8.8.8.8", record)
                read.assert_called_once_with()
                read.reset_mock()
                with self.assertRaisesRegex(ValueError, "capacity.json"):
                    hosted.validate_tenant(record)
                read.assert_called_once_with()

    def test_tenant_one_fits_beside_the_runner_and_passes_provisioning_checks(self):
        tenant = hosted.tenant_config(SUB)
        self.assertEqual(tenant["sub"], SUB)
        self.assertEqual(tenant["image_version"], "tenant-20261020-main-806a6d3-20261020T021255Z")
        self.assertEqual(tenant["vcpu_ceiling"], len(hosted.cpus(tenant["cpuset"])))
        self.assertFalse(hosted.cpus(tenant["cpuset"]) & hosted.cpus(hosted.WINDOWS_CPUS))
        # Both hardware threads of the same four cores (n and n+16).
        self.assertEqual({c % 16 for c in hosted.cpus(tenant["cpuset"])}, {8, 9, 10, 11})
        # The broker places a lease only where free_mem_mib covers its
        # memory_max(vcpus), so the guest must advertise memory_max of the
        # placeable N recorded with CLOUD's ceilings (7 vCPUs for this 28 GiB
        # template, 26,624 MiB advertised).
        placeable = placeable_vcpus(tenant["guest_ram_mib"])
        self.assertEqual(placeable, 7)
        self.assertTrue(cutover_rule(tenant["guest_ram_mib"], placeable))
        self.assertFalse(cutover_rule(tenant["guest_ram_mib"], placeable + 1))
        # 100 GiB: the build disk is shared with the runner's cache guard. The
        # in-VM runner's compile cache is then a quarter of it.
        self.assertEqual(tenant["disk_quota_bytes"], 100 * 1024**3)
        self.assertIn("--sccache-size-gib 25 ", hosted.load_provision().runner_args(tenant))
        self.assertEqual(tenant["resolvers"], hosted.host_config(HOST_ID)["resolvers"])
        # The tenant VM's uid must avoid every helper uid range.
        helper = hosted.helper_config(998, 998, "enp6s0", "8.8.8.8")
        uid = 400000 + tenant["tap_index"]
        self.assertFalse(helper["uid_base"] <= uid < helper["uid_base"] + len(helper["slots"]))
        job = helper["job_account_range"]
        self.assertFalse(job["base"] <= uid < job["base"] + job["count"])

    def test_no_host_id_or_subject_is_committed(self):
        self.assertNotIn("sub", json.loads(hosted.TENANT_FILE.read_text()))
        self.assertFalse(hasattr(hosted, "HOST_ID"))
        with self.assertRaises(TypeError):
            hosted.tenant_config()  # pylint: disable=no-value-for-parameter

    def test_tenant_sub_and_quota_are_overridable_and_still_validated(self):
        tenant = hosted.tenant_config(sub="org:01ARZ3NDEKTSV4RRFFQ69G5FAV", disk_quota_gib=400)
        self.assertEqual(tenant["sub"], "org:01ARZ3NDEKTSV4RRFFQ69G5FAV")
        self.assertEqual(tenant["disk_quota_bytes"], 400 * 1024**3)
        tenant = hosted.tenant_config(sub=SUB, image_version="next-image")
        self.assertEqual(tenant["image_version"], "next-image")
        with self.assertRaises(ValueError):
            hosted.tenant_config(sub=SUB, image_version="bad/image")
        for sub in ("acct:short", "01ARZ3NDEKTSV4RRFFQ69G5FAV", ""):
            with self.subTest(sub=sub), self.assertRaises(ValueError):
                hosted.tenant_config(sub=sub)

    def test_public_ipv4_is_discovered_never_private_or_ambiguous(self):
        output = ("2: eno1    inet 1.2.3.4/24 brd 1.2.3.255 scope global eno1\\       valid_lft forever\n"
                  "3: br0    inet 10.200.1.1/30 scope global br0\\       valid_lft forever\n")
        self.assertEqual(hosted.public_ipv4(output), "1.2.3.4")
        with self.assertRaises(ValueError):
            hosted.public_ipv4("3: br0    inet 10.200.1.1/30 scope global br0\n")
        with self.assertRaises(ValueError):
            hosted.public_ipv4(output + "4: eno2    inet 1.2.3.5/24 scope global eno2\n")

    def test_motor_host_unit_runs_unprivileged_after_the_helper_with_only_config_paths_writable(self):
        unit = (REPO / "crates/motor-host/systemd/motor-host.service").read_text()
        settings = {}
        for line in unit.splitlines():
            if "=" in line and not line.startswith("#"):
                key, value = line.split("=", 1)
                settings.setdefault(key, []).append(value)
        config = hosted.host_config(HOST_ID)
        self.assertEqual(settings["ExecStart"], [f"{hosted.HOST_BINARY} /etc/motor-host/host.json"])
        self.assertEqual(settings["User"], ["motor-host"])
        self.assertEqual(settings["LimitCORE"], ["0"])
        self.assertIn("motor-vm-helper.service", settings["Requires"][0].split())
        self.assertIn("motor-vm-helper.service", settings["After"][0].split())
        self.assertEqual(settings["ReadWritePaths"], [config["journal_dir"]])
        self.assertEqual(settings["RuntimeDirectory"], [Path(config["admin_socket"]).parent.name])
        self.assertEqual(Path(config["admin_socket"]).parent.parent, Path("/run"))
        for key, value in (("NoNewPrivileges", "yes"), ("PrivateTmp", "yes"), ("ProtectSystem", "strict"),
                           ("CapabilityBoundingSet", "")):
            self.assertEqual(settings[key], [value], key)
        helper = (REPO / "crates/motor-vm-helper/systemd/motor-vm-helper.service").read_text()
        self.assertIn(f"ExecStart={hosted.HELPER_BINARY} --config /etc/motor-vm-helper.json", helper)

    def test_renderer_cli_prints_the_same_json(self):
        output = subprocess.run(["python3", str(HERE / "hosted_host.py"), "host", "--host-id", HOST_ID, "--port", "9443"],
                                check=True, capture_output=True, text=True).stdout
        self.assertEqual(json.loads(output), hosted.host_config(HOST_ID, port=9443))
        tenant_output = renderer_cli("tenant", "--sub", SUB, "--image-version", "next-image")
        self.assertEqual(tenant_output.returncode, 0, tenant_output.stderr)
        self.assertEqual(json.loads(tenant_output.stdout)["image_version"], "next-image")
        for args in (["tenant", "--sub", "nope"], ["tenant"], ["host"]):
            with self.subTest(args=args):
                refused = renderer_cli(*args)
                self.assertEqual(refused.returncode, 2)


class CutoverRuleTests(unittest.TestCase):
    def test_placeable_n_comes_from_guest_ram_and_the_reserve(self):
        # The resized tenant 1: 78 GiB = 79,872 MiB, advertising at most 77,824
        # MiB, so max(16384, 3584 x N) <= 77,824 gives N = 21; CLOUD's ceilings
        # are max_job_vcpus 21 and max_job_mem_mib 75,264 (3584 x 21).
        self.assertEqual(TENANT_MEMORY_RESERVE_MIB, 2048)
        self.assertEqual(advertised_mib(79872), 77824)
        self.assertEqual(placeable_vcpus(79872), 21)
        self.assertEqual(memory_max_mib(21), 75264)
        self.assertTrue(cutover_rule(79872, 21))
        self.assertFalse(cutover_rule(79872, 22))
        # The rollback size, the old 28 GiB guest: 26,624 MiB places 7 vCPUs.
        self.assertEqual(placeable_vcpus(28672), 7)
        # Below the 16 GiB floor nothing places.
        self.assertEqual(placeable_vcpus(16384 + 2047), 0)


GIB = 1024**3
# The values /etc/motor-host/capacity.json holds on the build server.
CAPACITY = dict(windows_vm_bytes=16 * GIB, host_reserve_bytes=6 * GIB, jobs_min_bytes=24 * GIB,
                storage_margin_bytes=8 * GIB, growth_allowance_bytes=64 * GIB,
                tenant_memory_reserve_bytes=2 * GIB)
# The build server's measured MemTotal, 131,491,984 kB.
MEMTOTAL = 131491984 * 1024
# Tenant 1 after the cutover resize.
RESIZED = dict(vcpu_ceiling=24, cpuset="0-11,16-27", guest_ram_mib=79872, disk_quota_bytes=600 * GIB,
               max_weight=64, idle_suspend_s=0, memory_headroom_mib=2397)
UID = 998
# A provision.py module of its own, so the tests can point its paths and probes
# at a scratch tree.
provision = hosted.load_provision()


class Crash(BaseException):
    """Stops a resize at a hook point the way a killed process would."""


class InstalledStateTests(CapacityConfigTests):
    """install.sh --check (and --start) through check_installed, against a
    scratch host whose provision.py paths, capacity inputs and systemctl
    reads are doubles."""

    def setUp(self):
        super().setUp()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.capacity_error = None
        # Leave room for the resized fixture; the measured host is tested
        # separately because 78 GiB no longer fits its Windows/jobs budgets.
        self.memtotal = 160 * GIB
        self.limit = provision.jobs_slice_limits(CAPACITY)[0]
        self.slice = {"LoadState": "loaded", "MemoryMax": str(self.limit)}
        patch = mock.patch.multiple(
            provision, ROOT=self.root / "vm", REGISTRY=self.root / "registry", PROVISION=self.root / "provision",
            BINDINGS=self.root / "tenants.json", JOBS_DROP_IN=self.root / "jobs.conf",
            HELPER_CONFIG=self.root / "motor-vm-helper.json", HELPER_CGROUPS=self.root / "cgroup/tenants",
            read_capacity=self.read_capacity, memtotal_bytes=lambda: self.memtotal,
            slice_properties=lambda names: {name: self.slice[name] for name in names},
            runner_processes=lambda: [])
        patch.start()
        self.addCleanup(patch.stop)
        # No external command runs: not systemctl (doubled above), not mount.
        self.commands = []
        for target in (provision, provision.subprocess):
            recorder = mock.patch.object(target, "run", side_effect=lambda *a, **_k: self.commands.append(a))
            recorder.start()
            self.addCleanup(recorder.stop)
        for directory in (provision.REGISTRY, provision.PROVISION, provision.ROOT / "tenants"):
            directory.mkdir(parents=True)
        provision.JOBS_DROP_IN.write_text(provision.jobs_slice_render(CAPACITY))
        self.record_path = provision.PROVISION / "tenant-1.json"
        self.registry_path = provision.REGISTRY / "tenant-1.json"
        # A first install: the template, provisioned as provision.py would.
        self.install(hosted.tenant_config(SUB))

    def read_capacity(self):
        if self.capacity_error is not None:
            raise ValueError(self.capacity_error)
        return dict(self.capacity)

    def registry_entry(self, record):
        """The helper registry entry provision() writes for a record."""
        entry = {k: v for k, v in provision.checked(record).items() if k not in provision.PROVISION_ONLY}
        home = provision.ROOT / "tenants" / record["tenant"]
        entry.update({name: str(home / leaf) for name, leaf in provision.REGISTRY_PATHS.items()})
        return entry

    def install(self, record, registry=None, helper=None):
        self.record_path.write_text(hosted.render_json(record))
        self.registry_path.write_text(hosted.render_json(self.registry_entry(record) if registry is None else registry))
        provision.HELPER_CONFIG.write_text(
            hosted.render_json(hosted.helper_config(UID, UID, "enp6s0", "8.8.8.8", record)) if helper is None else helper)

    def resized(self):
        return {**hosted.tenant_config(SUB), **RESIZED}

    def problems(self):
        return hosted.check_installed(UID, UID, self.record_path, provision.HELPER_CONFIG, provision=provision)

    def assertFails(self, needle):
        problems = self.problems()
        self.assertTrue(any(needle in problem for problem in problems), (needle, problems))

    def files(self):
        return {path: path.read_bytes() for path in (self.record_path, self.registry_path, provision.HELPER_CONFIG)}

    def test_a_first_install_passes(self):
        self.assertEqual(self.problems(), [])
        self.assertEqual(json.loads(self.record_path.read_text()), {**json.loads(hosted.TENANT_FILE.read_text()), "sub": SUB})

    def test_no_windows_installed_state_reuses_one_capacity_read(self):
        self.capacity["windows_vm_bytes"] = 0
        self.slice["MemoryMax"] = str(provision.jobs_slice_limits(self.capacity)[0])
        provision.JOBS_DROP_IN.write_text(provision.jobs_slice_render(self.capacity))
        self.install(dict(self.resized(), cpuset="0-14,16-30", vcpu_ceiling=30))
        with mock.patch.object(provision, "read_capacity", wraps=self.read_capacity) as read:
            self.assertEqual(self.problems(), [])
            read.assert_called_once_with()

    def test_a_resized_record_passes_and_neither_check_nor_a_render_resets_it(self):
        record = self.resized()
        self.install(record)
        before = self.files()
        self.assertEqual(self.problems(), [])
        self.assertEqual(self.files(), before)
        # install.sh compares the installed files with these renders: the record
        # renders back as itself, and the helper keeps the resized cpuset.
        self.assertEqual(hosted.render_json(hosted.tenant_config(SUB, installed=self.record_path)),
                         self.record_path.read_text())
        helper = json.loads(provision.HELPER_CONFIG.read_text())
        self.assertEqual({slot["cpus"] for slot in helper["slots"]}, {"0-11,16-27"})
        self.assertEqual(self.commands, [])

    def test_a_hand_edited_cpuset_on_the_windows_cpus_fails(self):
        for cpuset, needle in (("8-15,24-31", "overlaps the Windows VM's CPUs 12-15,28-31"),
                               ("0-11,16-27,32", "beyond this host's 32 threads")):
            with self.subTest(cpuset=cpuset):
                record = dict(self.resized(), cpuset=cpuset)
                # Record, registry and helper edited alike, so only the cpuset rule
                # can fail.
                helper = json.loads(hosted.render_json(hosted.helper_config(UID, UID, "enp6s0", "8.8.8.8", self.resized())))
                for slot in helper["slots"]:
                    slot["cpus"] = cpuset
                self.install(record, helper=hosted.render_json(helper))
                self.assertFails(needle)

    def test_a_mismatch_on_any_shared_field_fails(self):
        record = self.resized()
        entry = self.registry_entry(record)

        def other(value):
            if isinstance(value, bool):
                return not value
            if isinstance(value, int):
                return value + 2
            return value + "0"

        for name in sorted(entry):
            with self.subTest(field=name):
                self.install(record, registry=dict(entry, **{name: other(entry[name])}))
                self.assertFails(f"differ at {name}")
        # A field only one side holds: a budget in the record the helper's
        # admission would not see, and one in the registry the guest would not.
        without = {k: v for k, v in entry.items() if k != "max_weight"}
        self.install(record, registry=without)
        self.assertFails("differ at max_weight")
        self.install({k: v for k, v in record.items() if k != "max_weight"}, registry=entry)
        self.assertFails("differ at max_weight")
        self.install(record)
        self.assertEqual(self.problems(), [])

    def test_a_helper_config_other_than_the_installed_render_fails(self):
        record = self.resized()
        template = hosted.render_json(hosted.helper_config(UID, UID, "enp6s0", "8.8.8.8"))
        changed = json.loads(hosted.render_json(hosted.helper_config(UID, UID, "enp6s0", "8.8.8.8", record)))
        changed["slots"][0]["io_weight"] = 20
        for label, helper in (("the template's cpuset", template), ("another slot weight", hosted.render_json(changed)),
                              ("another account", hosted.render_json(hosted.helper_config(UID + 1, UID, "enp6s0", "8.8.8.8", record)))):
            with self.subTest(label):
                self.install(record, helper=helper)
                self.assertFails("differs from the render of the installed tenant record")
        self.install(record)
        provision.HELPER_CONFIG.unlink()
        self.assertFails(str(provision.HELPER_CONFIG))

    def test_an_invalid_record_fails(self):
        self.install(self.resized())
        self.record_path.write_text(hosted.render_json(dict(self.resized(), max_weight=8)))
        self.assertFails("max_weight must be at least vcpu_ceiling")
        self.record_path.write_text("{")
        self.assertFails("cannot read the installed tenant record")

    def test_below_formula_headroom_reports_the_rewrite_remedy(self):
        self.record_path.write_text(hosted.render_json(dict(self.resized(), memory_headroom_mib=512)))
        self.assertFails("512 MiB is below minimum 2397 MiB for guest_ram_mib 79872 MiB")
        self.assertFails("run provision.py switch-image or resize to rewrite memory_headroom_mib")

    def test_measured_host_refuses_resized_tenant_with_new_headroom(self):
        self.install(self.resized())
        self.memtotal = MEMTOTAL
        self.assertFails("memory fit requires 144247357440 bytes, MemTotal is 134647791616 bytes")

    def test_capacity_fit_and_live_cap_are_checked(self):
        self.install(self.resized())
        self.assertEqual(self.problems(), [])
        self.capacity_error = "invalid capacity.json: not root-owned"
        self.assertFails("capacity.json: invalid capacity.json: not root-owned")
        self.capacity_error = None
        # RAM, headroom and 8 GiB page cache plus host reservations fit exactly,
        # but one byte less must refuse even though the page cache is reclaimable.
        needed = (79872 + 2397 + 8192) * 1048576 + 46 * GIB
        self.memtotal = needed
        self.assertEqual(self.problems(), [])
        self.memtotal = needed - 1
        self.assertFails(f"memory fit: memory fit requires {needed} bytes")
        self.memtotal = 160 * GIB
        for value in (str(self.limit + 1), "infinity"):
            with self.subTest(MemoryMax=value):
                self.slice = {"LoadState": "loaded", "MemoryMax": value}
                self.assertFails("jobs slice: motor-jobs.slice MemoryMax")
        self.slice = {"LoadState": "not-found", "MemoryMax": "infinity"}
        self.assertEqual(self.problems(), [])
        provision.JOBS_DROP_IN.write_text("[Slice]\nMemoryMax=1\n")
        self.assertFails("jobs slice: jobs-slice drop-in differs")
        provision.JOBS_DROP_IN.unlink()
        self.assertFails("jobs slice: missing or unreadable jobs-slice drop-in")

    def test_a_pending_resize_journal_fails(self):
        journal = provision.ROOT / "tenants" / "tenant-1" / "resize.json"
        journal.parent.mkdir(parents=True)
        journal.write_text("{}")
        self.assertFails(f"resize: pending resize journal: {journal}")
        journal.unlink()
        self.assertEqual(self.problems(), [])

    def provisioned_home(self):
        """The first-install tenant's disk, snapshots and image as provision()
        leaves them, on an image that can be resized."""
        record = hosted.tenant_config(SUB)
        home = provision.ROOT / "tenants" / record["tenant"]
        (home / "suspend").mkdir(parents=True)
        (home / "suspend" / "suspend.vmstate").write_bytes(b"old")
        (home / "generation").write_text("0\n")
        with (home / "disk.img").open("wb") as disk:
            disk.truncate(record["disk_quota_bytes"])
        image = provision.ROOT / "tenant-images" / record["image_version"]
        image.mkdir(parents=True)
        (image / "manifest.json").write_text(json.dumps({"version": record["image_version"],
                                                         "capabilities": ["sparse-v1", "resize-v1"]}))

    def resize(self, hook=None):
        def make_base(_config, _image, base):
            base.mkdir(parents=True)
            for name in ("base.vmstate", "base.memory"):
                (base / name).write_bytes(b"base")

        with mock.patch.object(provision.shutil, "disk_usage", return_value=mock.Mock(free=1 << 50)):
            arguments = {key: value for key, value in RESIZED.items() if key in provision.RESIZE_FIELDS}
            provision.resize("tenant-1", arguments, make_base=make_base, check_service=lambda _unit: "inactive",
                             vmms=lambda: [], hook=hook, output=lambda *_args: None)

    def test_a_cpuset_changing_resize_passes_check_directly(self):
        self.provisioned_home()
        self.resize()
        self.assertEqual(self.problems(), [])
        self.assertEqual(json.loads(self.record_path.read_text()), dict(self.resized(), page_cache_mib=8192))
        self.assertEqual(hosted.render_json(hosted.tenant_config(SUB, installed=self.record_path)),
                         self.record_path.read_text())
        self.assertEqual(self.commands, [])

    def test_a_resize_resumed_after_a_crash_between_records_and_helper_config_passes_check(self):
        self.provisioned_home()

        def crash(point):
            if point == "step:publish:registry":
                raise Crash(point)

        with self.assertRaises(Crash):
            self.resize(hook=crash)
        # The records are the new size and the helper config still the old: the
        # journal and the stale helper config both fail the check.
        self.assertEqual(json.loads(self.record_path.read_text()), dict(self.resized(), page_cache_mib=8192))
        self.assertFails("resize: pending resize journal")
        self.assertFails("differs from the render of the installed tenant record")
        self.resize()
        self.assertEqual(self.problems(), [])


class InstalledRenderTests(CapacityConfigTests):
    """The renders install.sh installs, with and without an installed record."""

    def setUp(self):
        super().setUp()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = Path(tmp.name) / "tenant-1.json"
        self.record = {**hosted.tenant_config(SUB), **RESIZED}
        self.path.write_text(hosted.render_json(self.record))

    def cli(self, *args):
        if args[0] == 'helper':
            args = (*args, '--uplink', 'enp6s0', '--public-ip', '8.8.8.8')
        return renderer_cli(*args)

    def test_an_installed_record_keeps_its_sizing(self):
        self.assertEqual(hosted.tenant_config(SUB, installed=self.path), self.record)
        rendered = self.cli("tenant", "--sub", SUB, "--installed", self.path)
        self.assertEqual(rendered.returncode, 0, rendered.stderr)
        self.assertEqual(rendered.stdout, self.path.read_text())
        helper = self.cli("helper", "--uid", UID, "--gid", UID, "--installed", self.path)
        self.assertEqual(helper.returncode, 0, helper.stderr)
        self.assertEqual(helper.stdout, hosted.render_json(hosted.helper_config(UID, UID, "enp6s0", "8.8.8.8", self.record)))
        self.assertEqual({slot["cpus"] for slot in json.loads(helper.stdout)["slots"]}, {"0-11,16-27"})

    def test_a_first_install_uses_the_template(self):
        template = json.loads(hosted.TENANT_FILE.read_text())
        rendered = self.cli("tenant", "--sub", SUB)
        self.assertEqual(rendered.returncode, 0, rendered.stderr)
        self.assertEqual(json.loads(rendered.stdout), {**template, "sub": SUB})
        helper = self.cli("helper", "--uid", UID, "--gid", UID)
        self.assertEqual(helper.returncode, 0, helper.stderr)
        self.assertEqual({slot["cpus"] for slot in json.loads(helper.stdout)["slots"]}, {template["cpuset"]})

    def test_a_first_install_can_override_the_image_and_sizing_without_a_subject_in_the_template(self):
        template = dict(self.record, image_version="fresh-main-image")
        del template["sub"]
        self.path.write_text(hosted.render_json(template))
        rendered = self.cli("tenant", "--sub", SUB, "--template", self.path)
        self.assertEqual(rendered.returncode, 0, rendered.stderr)
        self.assertEqual(json.loads(rendered.stdout), dict(template, sub=SUB))
        self.path.write_text(rendered.stdout)
        helper = self.cli("helper", "--uid", UID, "--gid", UID, "--installed", self.path)
        self.assertEqual(helper.returncode, 0, helper.stderr)
        self.assertEqual({slot["cpus"] for slot in json.loads(helper.stdout)["slots"]}, {"0-11,16-27"})

    def test_first_install_template_uses_windows_threads_without_a_windows_vm(self):
        self.capacity["windows_vm_bytes"] = 0
        template = dict(self.record, cpuset="0-14,16-30", vcpu_ceiling=30)
        del template["sub"]
        self.path.write_text(hosted.render_json(template))
        rendered = self.cli("tenant", "--sub", SUB, "--template", self.path)
        self.assertEqual(rendered.returncode, 0, rendered.stderr)
        self.assertEqual(json.loads(rendered.stdout), dict(template, sub=SUB))
        self.path.write_text(rendered.stdout)
        helper = self.cli("helper", "--uid", UID, "--gid", UID, "--installed", self.path)
        self.assertEqual(helper.returncode, 0, helper.stderr)
        self.assertEqual({slot["cpus"] for slot in json.loads(helper.stdout)["slots"]}, {"0-14,16-30"})

    def test_a_template_override_cannot_replace_an_installed_record(self):
        refused = self.cli("tenant", "--sub", SUB, "--installed", self.path, "--template", self.path)
        self.assertEqual(refused.returncode, 2)
        self.assertIn("keeps its sizing and image", refused.stderr)

    def test_a_first_install_template_still_passes_provisioning_checks(self):
        self.path.write_text(hosted.render_json(dict(self.record, vcpu_ceiling=0)))
        refused = self.cli("tenant", "--sub", SUB, "--template", self.path)
        self.assertEqual(refused.returncode, 2)
        self.assertIn("vcpu_ceiling", refused.stderr)

    def test_an_installed_record_refuses_template_overrides_and_another_subject(self):
        for args in (["--disk-quota-gib", "400"], ["--image-version", "next-image"]):
            with self.subTest(args=args):
                refused = self.cli("tenant", "--sub", SUB, "--installed", self.path, *args)
                self.assertEqual(refused.returncode, 2)
                self.assertIn("keeps its sizing and image", refused.stderr)
        refused = self.cli("tenant", "--sub", "org:01ARZ3NDEKTSV4RRFFQ69G5FAV", "--installed", self.path)
        self.assertEqual(refused.returncode, 2)
        self.assertIn("--tenant-sub differs", refused.stderr)
        self.path.write_text(hosted.render_json(dict(self.record, cpuset="8-15,24-31")))
        for kind in (["tenant", "--sub", SUB], ["helper", "--uid", UID, "--gid", UID]):
            with self.subTest(kind=kind[0]):
                refused = self.cli(*kind, "--installed", self.path)
                self.assertEqual(refused.returncode, 2)
                self.assertIn("overlaps the Windows VM's CPUs", refused.stderr)


class RunnerLogTests(unittest.TestCase):
    def test_request_uses_the_diagnostics_fields_motor_protocol_decodes(self):
        source = (REPO / "crates/motor-protocol/src/tenant.rs").read_text()
        variant = re.search(r"\n    Diagnostics \{(.*?)\n    \},", source, re.S)
        self.assertIsNotNone(variant)
        fields = {m[1] for m in re.finditer(r"^\s*(\w+):", variant[1], re.M)}
        request = hosted.runner_log_request("tenant-1", 1800000000, 1800000600, "00000000-0000-7000-8000-000000000001")
        self.assertEqual(request["op"], "diagnostics")
        self.assertLessEqual(set(request) - {"tenant", "op"}, fields)
        self.assertEqual(hosted.runner_log_request("tenant-1", 5, None, None),
                         {"tenant": "tenant-1", "op": "diagnostics", "since_unix_s": 5})
        with self.assertRaises(ValueError):
            hosted.runner_log_request("tenant-1", 10, 9, None)

    def test_times_are_unix_seconds_or_iso_and_default_to_utc(self):
        self.assertEqual(hosted.unix_seconds("1800000000"), 1800000000)
        self.assertEqual(hosted.unix_seconds("2026-10-23T14:00:00Z"), hosted.unix_seconds("2026-10-23T14:00:00"))
        self.assertEqual(hosted.unix_seconds("2026-10-23T16:00:00+02:00"), hosted.unix_seconds("2026-10-23T14:00:00"))

    def test_reply_lines_are_utc_stamped_and_refusals_are_errors(self):
        reply = {"ok": True, "journal": {"entries": [{"unix_us": 1800000000123456, "message": "runner_channel_closed reason=eof"}],
                                         "truncated": True}}
        self.assertEqual(hosted.runner_log_lines(reply), ["2027-01-15T08:00:00.123456+00:00 runner_channel_closed reason=eof",
                                                          "# truncated: narrow --since/--until or add --job"])
        for refused in ({"ok": False, "error": "admin_refused"}, {"ok": True, "journal": {"unavailable": True}}):
            with self.subTest(reply=refused), self.assertRaises(ValueError):
                hosted.runner_log_lines(refused)

    def test_admin_exchange_frames_one_request_and_reply(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "admin.sock"
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(str(path))
            server.listen(1)
            received = {}

            def answer():
                connection, _ = server.accept()
                with connection:
                    (length,) = struct.unpack(">I", connection.recv(4, socket.MSG_WAITALL))
                    received.update(json.loads(connection.recv(length, socket.MSG_WAITALL)))
                    body = json.dumps({"ok": True, "journal": {"entries": []}}).encode()
                    connection.sendall(struct.pack(">I", len(body)) + body)

            thread = threading.Thread(target=answer)
            thread.start()
            try:
                reply = hosted.admin_exchange({"tenant": "tenant-1", "op": "diagnostics", "since_unix_s": 1}, path)
            finally:
                thread.join(10)
                server.close()
        self.assertEqual(received, {"tenant": "tenant-1", "op": "diagnostics", "since_unix_s": 1})
        self.assertEqual(reply, {"ok": True, "journal": {"entries": []}})


class HostDrainTests(unittest.TestCase):
    def test_waits_for_an_echo_strictly_after_the_set_not_a_stale_or_equal_time(self):
        stale = {"ok": True, "draining": True, "broker_draining": True,
                 "broker_heartbeat_at_ms": 99}
        replies = [{"ok": True, "draining": True, "set_at_ms": 100}, stale,
                   dict(stale, broker_heartbeat_at_ms=100),
                   dict(stale, broker_draining=False, broker_heartbeat_at_ms=101),
                   dict(stale, broker_heartbeat_at_ms=102)]
        clock = [0.0]
        with mock.patch.object(hosted, "admin_exchange", side_effect=replies) as exchange, \
                mock.patch.object(hosted.time, "monotonic", side_effect=lambda: clock[0]), \
                mock.patch.object(hosted.time, "sleep", side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            result = hosted.drain_host(10)
        self.assertEqual(result["broker_heartbeat_at_ms"], 102)
        self.assertEqual([call.args[0] for call in exchange.call_args_list],
                         [{"op": "drain_host"}] + [{"op": "host_status"}] * 4)
        self.assertEqual(clock[0], 3)

    def test_timeout_without_a_new_echo_never_clears_the_drain(self):
        clock = [0.0]
        status = {"ok": True, "draining": True, "broker_draining": False,
                  "broker_heartbeat_at_ms": None}
        def answer(request, *args, **kwargs):
            return {"ok": True, "draining": True, "set_at_ms": 100} if request["op"] == "drain_host" else status
        with mock.patch.object(hosted, "admin_exchange", side_effect=answer) as exchange, \
                mock.patch.object(hosted.time, "monotonic", side_effect=lambda: clock[0]), \
                mock.patch.object(hosted.time, "sleep", side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            with self.assertRaisesRegex(ValueError, "timed out.*broker draining echo.*do not switch"):
                hosted.drain_host(2)
        self.assertEqual(clock[0], 2)
        self.assertNotIn({"op": "undrain_host"}, [call.args[0] for call in exchange.call_args_list])

    def test_clear_is_host_level_and_refusals_are_errors(self):
        with mock.patch.object(hosted, "admin_exchange", return_value={"ok": True, "draining": False}) as exchange:
            self.assertFalse(hosted.undrain_host()["draining"])
        exchange.assert_called_once_with({"op": "undrain_host"}, hosted.ADMIN_SOCKET)
        with mock.patch.object(hosted, "admin_exchange", return_value={"ok": False, "error": "admin_refused"}):
            with self.assertRaisesRegex(ValueError, "refused drain_host"):
                hosted.drain_host()
            with self.assertRaisesRegex(ValueError, "refused undrain_host"):
                hosted.undrain_host()

    def test_cli_reports_timeout_and_clear(self):
        with mock.patch.object(hosted.sys, "argv", ["hosted_host.py", "drain-host", "--timeout", "3"]), \
                mock.patch.object(hosted, "drain_host", side_effect=ValueError("timed out waiting for broker draining echo")) as drain, \
                mock.patch.object(hosted.sys, "stderr"):
            self.assertEqual(hosted.main(), 2)
        drain.assert_called_once_with(3)
        with mock.patch.object(hosted.sys, "argv", ["hosted_host.py", "undrain-host"]), \
                mock.patch.object(hosted, "undrain_host", return_value={"ok": True, "draining": False}) as clear, \
                mock.patch.object(hosted.sys, "stdout"):
            self.assertEqual(hosted.main(), 0)
        clear.assert_called_once_with()


class InstallScriptTests(CapacityConfigTests):
    def setUp(self):
        super().setUp()
        self.script = (HERE / "install.sh").read_text()

    def test_install_script_parses(self):
        subprocess.run(["bash", "-n", str(HERE / "install.sh")], check=True)

    def test_install_script_paths_are_the_rendered_config_paths(self):
        values = dict(re.findall(r"^([A-Z_]+)=(\S+)$", self.script, re.M))
        values = {k: re.sub(r"\$(\w+)", lambda m: values[m[1]], v) for k, v in values.items()}
        host = hosted.host_config(HOST_ID)
        self.assertEqual(values["HOST_BINARY"], str(hosted.HOST_BINARY))
        self.assertEqual(values["HELPER_BINARY"], str(hosted.HELPER_BINARY))
        self.assertEqual(values["LOADER"], hosted.helper_config(998, 998, "enp6s0", "8.8.8.8")["loader"])
        self.assertEqual(values["FIRECRACKER_DIR"] + "/firecracker", hosted.helper_config(998, 998, "enp6s0", "8.8.8.8")["firecracker"])
        self.assertEqual(values["SIGNING_KEY"], host["host_signing_key"])
        self.assertEqual(values["TLS_CERT"], host["certificate"])
        self.assertEqual(values["TLS_KEY"], host["private_key"])
        self.assertEqual(values["JOURNAL_DIR"], host["journal_dir"])
        self.assertEqual(values["BINDINGS"], host["bindings"])
        self.assertEqual(values["STORAGE"], host["storage"])
        self.assertEqual(values["HELPER_SOCKET"], host["helper_socket"])
        self.assertEqual(Path(values["KEY_DIR"]), Path(host["certificate"]).parent)

    def function(self, name):
        body = re.search(r"\n" + name + r"\(\) \{\n(.*?)\n\}\n", self.script, re.S)
        if body is None:
            self.fail(name)
        return body[0]

    def test_units_are_enabled_and_started_only_by_start_and_stopped_only_by_stop(self):
        start, stop = self.function("start_units"), self.function("stop_units")
        outside = self.script.replace(start, "").replace(stop, "")
        # Installing may restart journald to apply the host retention policy,
        # but never changes motor services or opens or closes the agent's port.
        self.assertEqual(outside.count('systemctl restart systemd-journald'), 1)
        outside = outside.replace('systemctl restart systemd-journald', '')
        self.assertIsNone(re.search(r"systemctl[^\n]*\b(start|restart|stop|enable|disable|--now)\b", outside))
        self.assertIsNone(re.search(r"\bufw (allow|delete|--force)", outside))
        self.assertEqual(re.findall(r"systemctl (enable --now [\w.-]+)", start),
                         ["enable --now motor-vm-helper.service", "enable --now motor-host.service"])
        self.assertEqual(re.findall(r"systemctl (\w+ --now [\w.-]+)", stop),
                         ["disable --now motor-host.service", "disable --now motor-vm-helper.service"])
        self.assertRegex(self.script, r'if \[ "\$mode" = start \]; then\n  start_units')
        self.assertRegex(self.script, r'if \[ "\$mode" = stop \]; then\n  stop_units')
        # Starting comes only after every check passed.
        self.assertLess(self.script.index('echo "$problems problem(s), $skipped declined'), self.script.index("then\n  start_units"))

    def test_identity_arguments_are_required_and_not_defaulted(self):
        self.assertIn("port='' host_id='' public_ip='' tenant_sub='' tenant_quota='' tenant_image=''", self.script)
        self.assertIn('[ -n "$host_id" ] && [ -n "$tenant_sub" ] || { echo "installing needs --host-id and --tenant-sub"', self.script)
        self.assertIn('[ -n "$uplink" ] || { echo "installing needs --uplink (no default)"', self.script)
        self.assertIn('uplink=$(installed "$HELPER_CONFIG" uplink)', self.script)

    def test_image_version_is_operator_managed_after_a_switch(self):
        # The installed record carries the image switch-image wrote; installing,
        # --check and --start render the tenant from that record, so none of
        # them passes an image.
        self.assertIn('tenant_image=$(installed "$TENANT_CONFIG" image_version)', self.script)
        self.assertNotIn("--image-version", self.script)
        self.assertIn('tenant image $image matches the helper registry', self.script)
        self.assertIn('TENANT_REGISTRY=/etc/motor-vm-helper/tenants/tenant-1.json', self.script)

    def test_an_installed_record_is_the_render_source_and_the_template_only_a_first_install(self):
        self.assertRegex(self.script, r'installed_record=\'\'\nif \[ "\$mode" != stop \] && \[ -f "\$TENANT_CONFIG" \]; then\n'
                                      r'  installed_record=\$TENANT_CONFIG\n')
        configs = self.function("configs")
        # Each render with its continuation lines.
        renders = re.findall(r"render (helper|tenant) ((?:[^\n]*\\\n)*[^\n]*)", configs)
        self.assertEqual([kind for kind, _ in renders], ["tenant", "helper"])
        self.assertIn('${installed_record:+--installed "$installed_record"}', renders[0][1])
        self.assertIn('${tenant_template:+--template "$tenant_template"}', renders[0][1])
        self.assertIn('--installed "$scratch/tenant.json"', renders[1][1])
        # --check and --start never re-derive the sizing from the template: the
        # disk quota is no longer read back to feed a template render.
        self.assertNotIn('tenant_quota=$(', self.script)

    def test_build_uses_rustup_when_there_is_no_runner_environment(self):
        build = self.function("build_binaries")
        self.assertIn('[ -f /srv/build/runner.env ] && source /srv/build/runner.env', build)
        self.assertIn('if ! command -v cargo >/dev/null && [ -f "$HOME/.cargo/env" ]; then source "$HOME/.cargo/env"; fi', build)

    def test_only_install_can_defer_a_missing_registry_until_provisioning(self):
        self.assertIn('elif [ "$check" = 0 ] && [ ! -e "$TENANT_REGISTRY" ]; then', self.function("preflight"))
        self.assertIn('tenant not provisioned yet; run provision.py before --check or --start', self.script)

    def test_check_and_start_verify_the_installed_state(self):
        configs = self.function("configs")
        self.assertIn('if [ "$check" = 1 ]; then\n    if render check-installed --uid "$uid" --gid "$gid"; then', configs)
        # The check flag is 1 for --check and --start; only install mode sets it
        # to 0, which lets place() replace config files.
        self.assertIn('check=1\n[ "$mode" = install ] && check=0', self.script)

    def test_installer_paths_are_the_provisioner_paths(self):
        values = dict(re.findall(r"^([A-Z_]+)=(\S+)$", self.script, re.M))
        self.assertEqual(values["TENANT_CONFIG"], str(hosted.TENANT_RECORD))
        self.assertEqual(values["HELPER_CONFIG"], str(hosted.HELPER_CONFIG))
        fresh = hosted.load_provision()
        self.assertEqual(fresh.PROVISION / "tenant-1.json", hosted.TENANT_RECORD)
        self.assertEqual(fresh.HELPER_CONFIG, hosted.HELPER_CONFIG)
        self.assertEqual(fresh.REGISTRY, hosted.REGISTRY)
        self.assertEqual(fresh.ROOT, hosted.STORAGE)
        self.assertEqual(fresh.WINDOWS_CPUS, hosted.WINDOWS_CPUS)

    def test_firewall_exception_is_the_one_inbound_policy_recognises(self):
        start = self.function("start_units")
        self.assertIn("ufw allow in proto tcp from any to 0.0.0.0/0 port \"$port\" comment motor-host-agent", start)
        self.assertIn("inbound_policy.py\" --host-agent-port \"$port\"", start)
        self.assertIn("# motor-host-agent", self.function("stop_units"))


if __name__ == "__main__":
    unittest.main()
