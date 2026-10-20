"""Run with python3 -m unittest -v script/hosted-host/test_hosted_host.py."""
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import unittest

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


class HostedHostConfigTests(unittest.TestCase):
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
        config = hosted.helper_config(998, 998)
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
            hosted.helper_config(0, 0)

    def test_tenant_one_fits_beside_the_runner_and_passes_provisioning_checks(self):
        tenant = hosted.tenant_config(SUB)
        self.assertEqual(tenant["sub"], SUB)
        self.assertEqual(tenant["image_version"], "tenant-20261020-main-806a6d3-20261020T021255Z")
        self.assertEqual(tenant["vcpu_ceiling"], len(hosted.cpus(tenant["cpuset"])))
        self.assertFalse(hosted.cpus(tenant["cpuset"]) & hosted.cpus(hosted.WINDOWS_CPUS))
        # Both hardware threads of the same four cores (n and n+16).
        self.assertEqual({c % 16 for c in hosted.cpus(tenant["cpuset"])}, {8, 9, 10, 11})
        # A full-size lease may use memory_max(vcpus); the guest must hold it.
        self.assertGreaterEqual(tenant["guest_ram_mib"], memory_max_mib(tenant["vcpu_ceiling"]))
        # 100 GiB: the build disk is shared with the runner's cache guard. The
        # in-VM runner's compile cache is then a quarter of it.
        self.assertEqual(tenant["disk_quota_bytes"], 100 * 1024**3)
        self.assertIn("--sccache-size-gib 25 ", hosted.load_provision().runner_args(tenant))
        self.assertEqual(tenant["resolvers"], hosted.host_config(HOST_ID)["resolvers"])
        # The tenant VM's uid must avoid every helper uid range.
        helper = hosted.helper_config(998, 998)
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
        for args in (["tenant", "--sub", "nope"], ["tenant"], ["host"]):
            with self.subTest(args=args):
                refused = subprocess.run(["python3", str(HERE / "hosted_host.py"), *args], capture_output=True, text=True)
                self.assertEqual(refused.returncode, 2)


class InstallScriptTests(unittest.TestCase):
    def setUp(self):
        self.script = (HERE / "install.sh").read_text()

    def test_install_script_parses(self):
        subprocess.run(["bash", "-n", str(HERE / "install.sh")], check=True)

    def test_install_script_paths_are_the_rendered_config_paths(self):
        values = dict(re.findall(r"^([A-Z_]+)=(\S+)$", self.script, re.M))
        values = {k: re.sub(r"\$(\w+)", lambda m: values[m[1]], v) for k, v in values.items()}
        host = hosted.host_config(HOST_ID)
        self.assertEqual(values["HOST_BINARY"], str(hosted.HOST_BINARY))
        self.assertEqual(values["HELPER_BINARY"], str(hosted.HELPER_BINARY))
        self.assertEqual(values["LOADER"], hosted.helper_config(998, 998)["loader"])
        self.assertEqual(values["FIRECRACKER_DIR"] + "/firecracker", hosted.helper_config(998, 998)["firecracker"])
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
        self.assertIsNotNone(body, name)
        return body[0]

    def test_units_are_enabled_and_started_only_by_start_and_stopped_only_by_stop(self):
        start, stop = self.function("start_units"), self.function("stop_units")
        outside = self.script.replace(start, "").replace(stop, "")
        # Installing and checking never enable, start or stop a unit, and never
        # open or close the agent's port.
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
        self.assertIn("port='' host_id='' public_ip='' tenant_sub='' tenant_quota=''", self.script)
        self.assertIn('[ -n "$host_id" ] && [ -n "$tenant_sub" ] || { echo "installing needs --host-id and --tenant-sub"', self.script)

    def test_firewall_exception_is_the_one_inbound_policy_recognises(self):
        start = self.function("start_units")
        self.assertIn("ufw allow in proto tcp from any to 0.0.0.0/0 port \"$port\" comment motor-host-agent", start)
        self.assertIn("inbound_policy.py\" --host-agent-port \"$port\"", start)
        self.assertIn("# motor-host-agent", self.function("stop_units"))


if __name__ == "__main__":
    unittest.main()
