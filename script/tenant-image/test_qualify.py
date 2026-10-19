#!/usr/bin/env python3
"""Portable qualification plumbing tests, never VM qualification evidence."""
import base64
import importlib.util
import json
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import unittest
from unittest import mock
import uuid

spec = importlib.util.spec_from_file_location("qualify", Path(__file__).with_name("qualify.py"))
assert spec is not None and spec.loader is not None
q = importlib.util.module_from_spec(spec)
spec.loader.exec_module(q)


def output(stream, data):
    return dict(type="output", stream=stream, bytes=base64.b64encode(data).decode())


def frames():
    return [output("stderr", b"err\n"), output("stdout", b"out\nTARGET /tenant/target 123\n"),
            dict(type="terminal", outcome=dict(type="exit", code=23))]


class QualificationTests(unittest.TestCase):
    def test_qualification_and_provisioning_use_the_same_trusted_bind_root(self):
        provision = q.module("qualification_layout_provision", q.HERE.parent / "tenant-provision/provision.py")
        self.assertEqual(q.ROOT, Path("/var/lib/motor-vm"))
        self.assertEqual(provision.ROOT, q.ROOT)

    def test_boot_args_preserve_manifest_and_deliver_opt_in_unit(self):
        original = "console=ttyS0 root=/dev/vda ro init=/sbin/motor-init net.ifnames=0"
        args = q.qualification_args(original)
        self.assertTrue(args.startswith(original + " "))
        self.assertIn("systemd.wants=motor-image-check.service", args.split())
        self.assertIn("systemd.wants=motor-qualification.service", args.split())
        credential = next(v.split(":", 1)[1] for v in args.split() if v.startswith("systemd.set_credential_binary="))
        unit = base64.b64decode(credential, validate=True).decode()
        self.assertEqual(unit, (q.HERE / "guest/motor-qualification.service").read_text())
        self.assertIn("StandardOutput=tty", unit)
        self.assertIn("TTYPath=/dev/ttyS0", unit)
        self.assertIn("ConditionKernelCommandLine=systemd.wants=motor-qualification.service", unit)
        self.assertNotIn("[Install]", unit)
        self.assertNotIn("systemd.wants=motor-qualification.service", original)
        self.assertLess(len(args.encode()), 4096)

    def test_normal_image_does_not_install_or_want_qualification_unit(self):
        installer = q.module("qualification_install_policy", q.HERE / "install-guest.py")
        with tempfile.TemporaryDirectory() as tmp:
            source, installed = Path(tmp) / "source", Path(tmp) / "installed"
            source.mkdir()
            production = ("motor-tenant-agent.service", "motor-net-rules.service", "motor-image-check.service")
            for name in (*production, "motor-qualification.service"):
                (source / name).write_text("fixture service\n")
            installer.install_services(source, installed)
            self.assertEqual(sorted(path.name for path in installed.iterdir()), sorted(production))
            self.assertFalse((installed / "motor-qualification.service").exists())
        for path in (q.HERE / "guest").glob("*.service"):
            if path.name != "motor-qualification.service":
                self.assertNotIn("motor-qualification", path.read_text())

    def test_bad_or_already_qualified_boot_args_refused(self):
        for args in ("", "a\nb", None, "x systemd.run=evil", "systemd.wants=motor-qualification.service"):
            with self.subTest(args=args), self.assertRaises(ValueError):
                q.qualification_args(args)

    def test_inventory_lines_match_only_complete_known_success(self):
        for expected in (q.LIVE, q.TENANT):
            self.assertTrue(q.exact_line("boot\r\n" + expected + "\r\n", expected))
            for text in (expected + " extra", "prefix " + expected, expected[:-1], expected.replace("passed", "failed")):
                self.assertFalse(q.exact_line(text, expected), text)
        self.assertFalse(q.exact_line("hosted image live inventory: 10 checks passed", q.LIVE))

    def test_guard_uses_170_gib_and_allocation_bytes(self):
        q.guard(q.FLOOR)
        q.guard(q.FLOOR + 123, 123)
        with self.assertRaisesRegex(RuntimeError, "170 GiB"):
            q.guard(q.FLOOR - 1)
        with self.assertRaises(RuntimeError):
            q.guard(q.FLOOR + 122, 123)

    def test_job_request_has_production_workspace_bundle_and_lease_limits(self):
        value = q.run_request("id", "commit", "tree", b"bundle", "exit 23")
        self.assertEqual(value["workspace_key"], q.WORKSPACE)
        self.assertEqual(value["repository_root"], q.WORKSPACE)
        self.assertEqual(value["weight_hint"], 1)
        self.assertEqual(value["timeout"], 30)
        self.assertEqual(value["bundles"][0]["length"], 6)
        self.assertEqual(value["bundles"][0]["sha256"], "1e6ed65d77d6364eeaed5a745ba5c4985ae2b700dd85d7cf7f027bdf294a33fc")

    def test_wire_framing_roundtrip_and_closed_or_oversized_frame(self):
        a, b = socket.socketpair()
        with a, b:
            q.send_frame(a, {"type": "hello", "protocol_version": 1})
            self.assertEqual(q.frame(b), {"type": "hello", "protocol_version": 1})
            a.sendall(struct.pack(">I", 0))
            with self.assertRaises(ValueError):
                q.frame(b)
            a.sendall(struct.pack(">I", 5) + b"xx")
            a.shutdown(socket.SHUT_WR)
            with self.assertRaises(EOFError):
                q.frame(b)

    def test_uuid_is_lowercase_version_seven_with_unique_random_bits(self):
        ids = [q.job_id() for _ in range(20)]
        self.assertEqual(len(set(ids)), 20)
        for identifier in ids:
            self.assertEqual(str(uuid.UUID(identifier)), identifier)
            self.assertEqual(uuid.UUID(identifier).version, 7)

    def test_result_requires_nonzero_exit_both_streams_and_exact_target(self):
        self.assertEqual(q.assert_job(frames(), 23, b"out\n", b"err\n"), (b"/tenant/target", b"123"))
        for bad in (frames()[:-1], frames() + frames()[-1:],
                    [*frames()[:-1], dict(type="terminal", outcome=dict(type="exit", code=0))],
                    [output("stdout", b"out\nTARGET /tenant/target 123\n"), frames()[-1]],
                    [output("stdout", b"out\nTARGET relative 123\n"), output("stderr", b"err\n"), frames()[-1]]):
            with self.subTest(frames=bad), self.assertRaises(RuntimeError):
                q.assert_job(bad, 23, b"out\n", b"err\n")

    def test_output_chunks_keep_stdout_and_stderr_separate(self):
        value = [output("stdout", b"ou"), output("stderr", b"er"),
                 output("stdout", b"t\nTARGET /x 45\n"), output("stderr", b"r\n"), frames()[-1]]
        self.assertEqual(q.assert_job(value, 23, b"out\n", b"err\n"), (b"/x", b"45"))

    def test_failure_never_skips_teardown_or_promotes_unreached_steps(self):
        with tempfile.TemporaryDirectory() as tmp:
            harness = object.__new__(q.Harness)
            harness.results = Path(tmp)
            harness.verdicts = {}
            harness.name = "portable-fixture"
            harness.log = (Path(tmp) / "raw.jsonl").open("w")
            harness.preflight = mock.Mock(side_effect=RuntimeError("fixture missing test binary"))
            harness.teardown = mock.Mock(return_value="all owned resources gone")
            with mock.patch("builtins.print"):
                self.assertEqual(harness.execute(), 1)
            harness.teardown.assert_called_once()
            summary = json.loads((Path(tmp) / "summary.json").read_text())
            self.assertEqual(summary["verdict"], "FAIL")
            self.assertTrue(summary["steps"]["teardown"])
            self.assertFalse(summary["steps"]["vsock isolation"])
            self.assertFalse(summary["steps"]["post-restore job"])
            self.assertEqual(set(summary["steps"]), set(q.STEPS))

    def test_console_success_cannot_hide_a_guest_payload_failure(self):
        harness = object.__new__(q.Harness)
        harness.console = lambda: q.TENANT + "\nFAIL guest qualification: isolation failed\n"
        with self.assertRaisesRegex(RuntimeError, "isolation failed"):
            harness.expect_console(q.TENANT)

    def test_empty_exception_still_names_the_type_and_preserves_traceback(self):
        with tempfile.TemporaryDirectory() as tmp:
            harness = object.__new__(q.Harness)
            harness.results = Path(tmp)
            harness.verdicts = {}
            harness.name = "portable-fixture"
            harness.log = (Path(tmp) / "raw.jsonl").open("w")
            harness.preflight = mock.Mock(side_effect=EOFError())
            harness.teardown = mock.Mock(return_value="clean")
            with mock.patch("builtins.print"):
                self.assertEqual(harness.execute(), 1)
            summary = json.loads((Path(tmp) / "summary.json").read_text())
            self.assertEqual(summary["error"], "EOFError: EOFError()")
            events = [json.loads(line) for line in (Path(tmp) / "raw.jsonl").read_text().splitlines()]
            failed = next(row for row in events if row.get("step") == "preflight")
            self.assertEqual(failed["exception_type"], "EOFError")
            self.assertIn("EOFError", failed["traceback"])
            self.assertNotEqual(failed["error"], "")

    def test_subprocess_failure_keeps_real_exit_status_stdout_and_stderr(self):
        error = subprocess.CalledProcessError(137, ["jailer", "--id", "base"], output=b"raw stdout\n", stderr=b"raw stderr\n")
        evidence = q.error_evidence(error)
        self.assertEqual(evidence["returncode"], 137)
        self.assertEqual(evidence["stdout"], "raw stdout\n")
        self.assertEqual(evidence["stderr"], "raw stderr\n")
        self.assertEqual(evidence["command"], ["jailer", "--id", "base"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
