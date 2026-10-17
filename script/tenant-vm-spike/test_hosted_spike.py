#!/usr/bin/env python3
"""Portable tests for the measurement plumbing, not evidence of VM behavior."""

import importlib.util
import base64
import json
from pathlib import Path
import socket
import struct
import subprocess
import platform
import tempfile
import time
import threading
import unittest
from unittest.mock import Mock, patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("hosted_spike", Path(__file__).with_name("hosted_spike.py"))
assert spec is not None and spec.loader is not None
spike = importlib.util.module_from_spec(spec)
spec.loader.exec_module(spike)
config_spec = importlib.util.spec_from_file_location("kernel_config", Path(__file__).with_name("check-kernel-config.py"))
assert config_spec is not None and config_spec.loader is not None
kernel_config = importlib.util.module_from_spec(config_spec)
config_spec.loader.exec_module(kernel_config)
guest_spec = importlib.util.spec_from_file_location("guest", Path(__file__).with_name("guest.py"))
assert guest_spec is not None and guest_spec.loader is not None
guest = importlib.util.module_from_spec(guest_spec)
guest_spec.loader.exec_module(guest)


def complete_samples():
    return {name: [100, 102, 101, 99, 103] for name in spike.REQUIRED_METRICS}


class HostedSpikeTests(unittest.TestCase):
    def test_resolved_kernel_config_accepts_required_features_without_tunnels(self):
        text = "\n".join(f"CONFIG_{name}=y" for name in kernel_config.REQUIRED)
        self.assertEqual(kernel_config.check(text), [])

    def test_resolved_kernel_config_rejects_fallback_tunnel(self):
        text = "\n".join(f"CONFIG_{name}=y" for name in kernel_config.REQUIRED) + "\nCONFIG_IPV6_SIT=y"
        self.assertEqual(kernel_config.check(text), [
            "CONFIG_IPV6_SIT must be unset: namespace fallback interfaces are not allowed"])

    def test_resolved_kernel_config_requires_cgroup_bpf(self):
        text = "\n".join(f"CONFIG_{name}=y" for name in kernel_config.REQUIRED if name != "CGROUP_BPF")
        self.assertEqual(kernel_config.check(text), ["CONFIG_CGROUP_BPF must be built in"])

    def test_guest_diagnostics_finds_runtime_unit_by_job_sandbox_not_job_id(self):
        job_id = "00000000-0000-4000-8000-000000000001"

        def run(args, **_kwargs):
            if "list-units" in args:
                output = "motor-job-runtime.service loaded active running Job\n"
            elif "--property=RootDirectory" in args:
                output = f"/tenant/state/spike/sandboxes/{job_id}/root\n"
            elif "--value" in args:
                output = "123\n"
            elif args[0] == "journalctl":
                output = "systemd: BPF available\n"
            else:
                output = "PrivateNetwork=yes\nPrivateDevices=yes\nMainPID=123\n"
            return SimpleNamespace(returncode=0, stdout=output, stderr="")

        with patch.object(guest.subprocess, "run", side_effect=run), patch.object(guest.os, "readlink", return_value="net:[123]"):
            result = guest.diagnostics(job_id)
        self.assertEqual(result["unit"], "motor-job-runtime.service")
        self.assertIn("PrivateNetwork=yes", result["properties"]["stdout"])
        self.assertEqual(result["network_namespaces"]["job_main"], "net:[123]")

    def test_isolated_probe_decodes_output_bytes_and_collects_unit_diagnostics(self):
        a, b = socket.socketpair()
        errors, requests = [], []

        def server():
            try:
                with b:
                    b.settimeout(5)
                    spike.read_frame(b)
                    spike.send_frame(b, {"type": "hello", "protocol_version": 1})
                    requests.append(spike.read_frame(b))
                    spike.send_frame(b, {"type": "heartbeat"})
                    spike.send_frame(b, {"type": "accepted"})
                    spike.read_frame(b)
                    spike.send_frame(b, {"type": "started", "seq": 0})
                    spike.send_frame(b, {"type": "output", "seq": 1, "stream": "stdout",
                        "bytes": base64.b64encode(b"SPIKE_ISOLATED_JOB_PASS\n").decode()})
                    spike.send_frame(b, {"type": "terminal", "outcome": {"type": "exit", "code": 0}})
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=server, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                harness = Mock(work=Path(directory))
                vm = Mock()
                vm.connect.return_value = a
                with patch.object(spike, "fixture_bundle", return_value=("0" * 40, "1" * 40, b"bundle")):
                    result = spike.isolated_job(harness, vm)
                self.assertEqual(result["output"], "SPIKE_ISOLATED_JOB_PASS\n")
                vm.control.assert_called_once_with("diagnostics", job_id=requests[0]["job_id"])
        finally:
            a.close()
            worker.join(timeout=5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])

    def test_fixture_bundle_contains_only_the_requested_snapshot_ref(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repository = root / "repo"
            evidence = spike.Evidence(root)
            try:
                evidence.run("git", "init", "--quiet", repository)
                (repository / "input").write_text("probe\n")
                evidence.run("git", "-C", repository, "add", "input")
                evidence.run("git", "-C", repository, "-c", "user.name=Probe",
                             "-c", "user.email=probe@example.invalid", "-c", "commit.gpgsign=false",
                             "commit", "--quiet", "-m", "probe")
                job_id = "00000000-0000-4000-8000-000000000001"
                commit, tree, bundle = spike.fixture_bundle(evidence, repository, job_id)
                heads = evidence.run("git", "bundle", "list-heads", repository / "input.bundle")
                self.assertEqual(heads, f"{commit} refs/ck-motor/{job_id}/snapshot")
                self.assertEqual(tree, evidence.run("git", "-C", repository, "rev-parse", "HEAD^{tree}"))
                self.assertTrue(bundle.startswith((b"# v2 git bundle\n", b"# v3 git bundle\n")))
                self.assertEqual(evidence.run("git", "-C", repository, "for-each-ref", "refs/ck-motor/"), "")
            finally:
                evidence.raw.close()

    @unittest.skipUnless(platform.system() == "Linux" and platform.machine() == "x86_64", "Linux kernel builder")
    def test_kernel_builder_refuses_unpinned_compiler_before_download(self):
        with tempfile.TemporaryDirectory() as directory:
            compiler = Path(directory) / "gcc-unpinned"
            compiler.write_text("#!/bin/sh\necho '15.2.0'\n")
            compiler.chmod(0o755)
            import os
            result = subprocess.run(["bash", str(Path(__file__).with_name("build-kernel.sh")),
                                     str(Path(directory) / "output")],
                                    env={**os.environ, "CC": str(compiler)}, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("kernel compiler must be pinned GCC 13.4.0", result.stdout)
            self.assertFalse((Path(directory) / "output/linux-6.1.128.tar.xz").exists())

    def test_frame_roundtrip(self):
        a, b = socket.socketpair()
        with a, b:
            spike.send_frame(a, {"type": "hello", "protocol_version": 1})
            self.assertEqual(spike.read_frame(b), {"type": "hello", "protocol_version": 1})

    def test_frame_short_read_and_eof(self):
        a, b = socket.socketpair()
        with a, b:
            a.sendall(struct.pack(">I", 4) + b"{")
            a.shutdown(socket.SHUT_WR)
            with self.assertRaisesRegex(RuntimeError, "unexpected EOF"):
                spike.read_frame(b)

    def test_frame_reads_skip_heartbeats_before_the_real_reply(self):
        a, b = socket.socketpair()
        with a, b:
            spike.send_frame(a, {"type": "heartbeat"})
            spike.send_frame(a, {"type": "heartbeat", "queue_wait_ms": 0})
            spike.send_frame(a, {"type": "accepted", "job_id": "probe"})
            self.assertEqual(spike.read_frame(b), {"type": "accepted", "job_id": "probe"})

    def test_heartbeats_cannot_extend_the_absolute_frame_deadline(self):
        a, b = socket.socketpair()
        with a, b:
            for _ in range(10):
                spike.send_frame(a, {"type": "heartbeat"})
            with self.assertRaises((TimeoutError, socket.timeout)):
                spike.read_frame(b, deadline=time.monotonic() + 0.01)
            self.assertIsNone(b.gettimeout())

    def test_frame_rejects_oversized_message(self):
        a, b = socket.socketpair()
        with a, b:
            a.sendall(struct.pack(">I", 1048577))
            with self.assertRaisesRegex(RuntimeError, "invalid frame size"):
                spike.read_frame(b)

    def test_snapshot_load_has_network_override_and_ordinary_file_memory(self):
        vm = spike.VM.__new__(spike.VM)
        vm.api = Mock(return_value={"status": 204})
        vm.load("base", "tap-b")
        vm.api.assert_called_once_with("PUT", "/snapshot/load", {
            "snapshot_path": "/state/base.vmstate",
            "mem_backend": {"backend_type": "File", "backend_path": "/state/base.mem"},
            "enable_diff_snapshots": False, "resume_vm": False,
            "network_overrides": [{"iface_id": "eth0", "host_dev_name": "tap-b"}],
        })

    def test_api_sends_http_json_and_closes_connection(self):
        vm = spike.VM.__new__(spike.VM)
        vm.e = Mock()
        vm.socket_root = Path("/unused")
        a, b = socket.socketpair()
        with a, b:
            b.sendall(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            with patch.object(spike.UnixHTTP, "connect", lambda client: setattr(client, "sock", a)):
                self.assertEqual(vm.api("PATCH", "/vm", {"state": "Paused"}), {"status": 204, "body": ""})
            self.assertEqual(a.fileno(), -1)
            request = b.recv(4096)
            self.assertIn(b"PATCH /vm HTTP/1.1", request)
            self.assertIn(b'{"state": "Paused"}', request)

    def test_snapshot_create_is_full_and_fsyncs_both_files_and_directory(self):
        vm = spike.VM.__new__(spike.VM)
        vm.e = Mock()
        vm.io_counters = Mock(return_value={"wchar": 0, "write_bytes": 0})
        vm.api = Mock(return_value={"status": 204})
        with tempfile.TemporaryDirectory() as directory:
            vm.h = Mock(state=Path(directory))
            for name in ["base.mem", "base.vmstate"]:
                (Path(directory) / name).touch()
            with patch.object(spike.os, "fsync") as sync:
                self.assertEqual(vm.snapshot("base"), {"status": 204})
                self.assertEqual(sync.call_count, 3)
        vm.api.assert_called_once_with("PUT", "/snapshot/create", {"snapshot_type": "Full",
            "snapshot_path": "/state/base.vmstate", "mem_file_path": "/state/base.mem"})
        self.assertEqual(vm.e.record.call_args.args[0], "snapshot_timing")
        self.assertGreaterEqual(vm.e.record.call_args.kwargs["api_ms"], 0)
        self.assertGreaterEqual(vm.e.record.call_args.kwargs["fsync_ms"], 0)
        for name in ["memory_file_fsync_ms", "vmstate_file_fsync_ms", "directory_fsync_ms"]:
            self.assertGreaterEqual(vm.e.record.call_args.kwargs[name], 0)

    def test_network_override_requires_packets_on_replacement_tap(self):
        harness = spike.Harness.__new__(spike.Harness)
        harness.tap_rx = Mock(side_effect=[10, 10])
        vm = Mock()
        vm.control.return_value = {"ping": "1 received"}
        with self.assertRaisesRegex(RuntimeError, "did not send packets through tap-b"):
            harness.check_network_override(vm)
        vm.control.assert_called_once_with("network", profile="b")

    def test_network_override_accepts_observed_replacement_tap_packets(self):
        harness = spike.Harness.__new__(spike.Harness)
        harness.tap_rx = Mock(side_effect=[10, 12])
        vm = Mock()
        vm.control.return_value = {"ping": "1 received"}
        result = harness.check_network_override(vm)
        self.assertEqual(result["tap_b_rx_after"], 12)

    def test_complete_five_run_report_uses_median(self):
        result = spike.summarize(complete_samples())
        self.assertEqual(result["verdict"], "go")
        self.assertEqual(result["metrics"]["cold_boot"]["p50"], 101)
        self.assertEqual(result["metrics"]["cold_boot"]["n"], 5)

    def test_missing_measurement_replans(self):
        samples = complete_samples()
        del samples["ficlone_24g"]
        result = spike.summarize(samples)
        self.assertEqual(result["verdict"], "replan")
        self.assertIn("ficlone_24g", " ".join(result["reasons"]))

    def test_four_runs_cannot_pass_as_five(self):
        samples = complete_samples()
        samples["cold_boot"].pop()
        self.assertEqual(spike.summarize(samples)["verdict"], "replan")

    def test_suspend_restore_over_budget_replans(self):
        samples = complete_samples()
        samples["suspend_restore"] = [4999, 5001, 5002, 5003, 5004]
        self.assertEqual(spike.summarize(samples)["verdict"], "replan")

    def test_suspend_restore_at_budget_is_allowed(self):
        samples = complete_samples()
        samples["suspend_restore"] = [5000] * 5
        self.assertEqual(spike.summarize(samples)["verdict"], "go")

    def test_restore_headroom_breach_replans(self):
        samples = complete_samples()
        samples["restore_memory_peak_bytes"][-1] = 19 * spike.GIB
        self.assertEqual(spike.summarize(samples)["verdict"], "replan")

    def test_an_error_cannot_produce_go(self):
        self.assertEqual(spike.summarize(complete_samples(), "drive substitution failed")["verdict"], "replan")

    def test_raw_samples_preserve_command_and_unit(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = spike.Evidence(Path(directory))
            evidence.record("invocation", argv=["hosted_spike.py", "--work", "work"])
            evidence.sample("restore_memory_peak_bytes", 123456, unit="bytes")
            evidence.raw.close()
            rows = [json.loads(line) for line in (Path(directory) / "raw.jsonl").read_text().splitlines()]
            self.assertEqual(rows[0]["argv"][0], "hosted_spike.py")
            self.assertEqual(rows[1]["unit"], "bytes")
            self.assertEqual(rows[1]["value"], 123456)


if __name__ == "__main__":
    unittest.main(verbosity=2)
