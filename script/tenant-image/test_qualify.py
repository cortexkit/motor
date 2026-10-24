#!/usr/bin/env python3
"""Portable qualification plumbing tests, never VM qualification evidence."""
import base64
import importlib.util
import hashlib
import json
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
from types import SimpleNamespace
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
    def test_qualification_cannot_check_an_image_with_different_parity_inputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = root / 'repo'
            (repo / 'script/runner').mkdir(parents=True)
            hashes = {}
            for name in q.PARITY_FILES:
                relative = 'script/runner/' + name
                (repo / relative).write_text(name)
                hashes[relative] = hashlib.sha256(name.encode()).hexdigest()
            (root / 'manifest.json').write_text(json.dumps({'versions': {'runner_inputs_sha256': hashes}}))
            (root / 'tools.json').write_text(json.dumps({'parity': {'observed': {'tools': {'gnutest': {'sha256': 'a' * 64}}}}}))
            self.assertEqual('a' * 64, q.parity_inputs(root, repo))
            (repo / 'script/runner/toolchain.json').write_text('changed contract')
            with self.assertRaisesRegex(RuntimeError, 'parity input differs.*toolchain.json'):
                q.parity_inputs(root, repo)

    def test_real_job_parity_command_checks_and_propagates_drift(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            checker = root / 'toolchain-parity.py'
            checker.write_text('import sys\nassert sys.argv[1:6] == ["--check", "--role", "tenant", "--job", "--gnu-test-sha256"]\nprint("parity measured")\nsys.exit(7)\n')
            command = f'd="{root}"; ' + q.parity_job_command('a' * 64, 1) + 'echo false-success'
            result = subprocess.run(['sh', '-c', command], cwd=root, capture_output=True, text=True)
            self.assertEqual(1, result.returncode)
            self.assertEqual('parity measured\n', result.stderr)
            self.assertEqual('', result.stdout)
            checker.write_text(checker.read_text().replace('sys.exit(7)', 'sys.exit(0)'))
            result = subprocess.run(['sh', '-c', command], cwd=root, capture_output=True, text=True)
            self.assertEqual(0, result.returncode)
            self.assertEqual('false-success\n', result.stdout)
        with self.assertRaisesRegex(ValueError, 'package-qualified'):
            q.parity_job_command('', 1)

    def test_javascript_setup_runs_in_the_booted_guest_and_in_every_job(self):
        guest = q.module('javascript_guest', q.HERE / 'guest/hosted_qualify.py')
        self.assertEqual(q.JAVASCRIPT_SETUP, guest.JAVASCRIPT_SETUP)
        self.assertLess(q.STEPS.index("toolchain parity"), q.STEPS.index("javascript setup"))
        self.assertLess(q.STEPS.index("javascript setup"), q.STEPS.index("first job"))
        with tempfile.TemporaryDirectory() as tmp:
            scratch = Path(tmp)

            def runner(*args):
                Path(args[-1]).mkdir()
                return guest.RUNNER_JAVASCRIPT_PASS + '\n'
            with mock.patch.object(guest, 'run', side_effect=runner) as run, \
                 mock.patch('builtins.print') as printed:
                guest.javascript_setup_probe(scratch)
            self.assertEqual(('/usr/local/bin/motor-runner', 'check-javascript-setup', '--tenant-vm',
                              '--scratch', str(scratch / 'javascript-setup')), run.call_args.args)
            printed.assert_called_once_with(q.JAVASCRIPT_SETUP, flush=True)
            with mock.patch.object(guest, 'run', side_effect=lambda *a: Path(a[-1]).mkdir() or 'FAIL\n'), \
                 self.assertRaisesRegex(RuntimeError, 'did not pass'):
                guest.javascript_setup_probe(scratch)
            # Each job's repository carries all three lockfile kinds, and the
            # job fails unless setup installed them without lifecycle scripts.
            fixture = scratch / 'fixture'
            fixture.mkdir()
            q.stage_javascript_fixtures(fixture)
            for manager, lockfile in (('bun', 'bun.lock'), ('npm', 'package-lock.json'), ('pnpm', 'pnpm-lock.yaml')):
                self.assertTrue((fixture / manager / lockfile).is_file())
                self.assertFalse((fixture / manager / 'node_modules').exists())
            command = 'set -eu; ' + q.javascript_job_command() + 'echo installed'
            result = subprocess.run(['sh', '-c', command], cwd=fixture, capture_output=True, text=True)
            self.assertEqual(1, result.returncode)
            self.assertEqual('runner setup did not install the bun fixture\n', result.stderr)
            for manager in q.JAVASCRIPT_MANAGERS:
                (fixture / manager / 'node_modules/fixture-dep').mkdir(parents=True)
                (fixture / manager / 'node_modules/fixture-dep/index.js').write_text('')
            result = subprocess.run(['sh', '-c', command], cwd=fixture, capture_output=True, text=True)
            self.assertEqual('installed\n', result.stdout)
            (fixture / 'pnpm/lifecycle-ran').write_text('ran')
            result = subprocess.run(['sh', '-c', command], cwd=fixture, capture_output=True, text=True)
            self.assertEqual('the pnpm fixture lifecycle script ran\n', result.stderr)

    def test_trusted_cargo_fetch_runs_online_in_a_helper_with_a_cold_cache(self):
        guest = q.module('cargo_fetch_guest', q.HERE / 'guest/hosted_qualify.py')
        self.assertEqual(q.CARGO_FETCH, guest.CARGO_FETCH)
        self.assertLess(q.STEPS.index("javascript setup"), q.STEPS.index("trusted cargo fetch"))
        self.assertLess(q.STEPS.index("trusted cargo fetch"), q.STEPS.index("first job"))
        with tempfile.TemporaryDirectory() as tmp:
            scratch = Path(tmp)
            target = scratch / 'cargo-fetch'
            target.mkdir()
            (target / 'warm-cache').write_text('must not be reused')

            def runner(*args):
                self.assertFalse(target.exists())
                target.mkdir()
                return guest.RUNNER_CARGO_FETCH_PASS + '\n'
            with mock.patch.object(guest, 'run', side_effect=runner) as run, \
                 mock.patch('builtins.print') as printed:
                guest.cargo_fetch_probe(scratch)
            self.assertEqual(('/usr/local/bin/motor-runner', 'check-tenant-cargo-fetch',
                              '--scratch', str(target)), run.call_args.args)
            self.assertFalse(target.exists())
            printed.assert_called_once_with(q.CARGO_FETCH, flush=True)
            with mock.patch.object(guest, 'run', return_value='FAIL\n'), \
                 self.assertRaisesRegex(RuntimeError, 'did not pass'):
                guest.cargo_fetch_probe(scratch)
            with mock.patch.object(guest, 'run', side_effect=RuntimeError('DNS failed')), \
                 self.assertRaisesRegex(RuntimeError, 'DNS failed'):
                guest.cargo_fetch_probe(scratch)

    def test_guest_host_parity_must_match_image_gnutest_inventory(self):
        guest = q.module('parity_guest', q.HERE / 'guest/hosted_qualify.py')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'gnutest.sha256').write_text('a' * 64)
            report = {'passed': True, 'observed': {'tools': {'gnutest': {'sha256': 'a' * 64}}}}
            with mock.patch.object(guest, 'run', return_value=json.dumps(report)) as run, mock.patch('builtins.print'):
                guest.toolchain_probe(root)
            self.assertEqual(('python3', str(root / 'toolchain-parity.py'), '--check', '--role', 'tenant'), run.call_args.args)
            self.assertEqual(report, json.loads((root / 'parity-host.json').read_text()))
            report['observed']['tools']['gnutest']['sha256'] = 'b' * 64
            with mock.patch.object(guest, 'run', return_value=json.dumps(report)), self.assertRaisesRegex(RuntimeError, 'gnutest differs'):
                guest.toolchain_probe(root)

    def test_program_staging_copies_bytes_not_symlink_or_writable_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "built-program"
            source.write_bytes(b"reviewed program\n")
            source.chmod(0o777)
            link = root / "loader-link"
            link.symlink_to(source)
            destination = root / "trusted/bin/loader"
            own = mock.Mock()
            evidence = q.stage_program(link, destination, own=own)
            self.assertEqual(destination.read_bytes(), b"reviewed program\n")
            self.assertFalse(destination.is_symlink())
            self.assertEqual(destination.stat().st_mode & 0o7777, 0o755)
            self.assertEqual(source.stat().st_mode & 0o7777, 0o777)
            own.assert_any_call(destination.parent, 0, 0)
            own.assert_any_call(destination, 0, 0)
            self.assertEqual(evidence["mode"], "0o755")
            with self.assertRaises(FileExistsError):
                q.stage_program(source, destination, own=own)

    def test_not_loaded_unit_is_idempotent_but_access_denied_is_not(self):
        harness = object.__new__(q.Harness)
        harness.record = mock.Mock()
        gone = subprocess.CompletedProcess([], 5, stdout=b"", stderr=b"Unit gone.service not loaded.\n")
        with mock.patch.object(q.subprocess, "run", return_value=gone):
            self.assertEqual(harness.run("systemctl", "stop", "gone.service", allow_missing_unit=True), b"")
            with self.assertRaises(RuntimeError):
                harness.run("systemctl", "stop", "gone.service")
        denied = subprocess.CompletedProcess([], 1, stdout=b"", stderr=b"Access denied\n")
        with mock.patch.object(q.subprocess, "run", return_value=denied), self.assertRaises(RuntimeError):
            harness.run("systemctl", "stop", "gone.service", allow_missing_unit=True)

    def test_helper_exit_reports_state_and_exact_journal_without_timeout(self):
        harness = object.__new__(q.Harness)
        harness.run = mock.Mock(side_effect=[b"LoadState=not-found\nActiveState=inactive\nExecMainStatus=1\n",
                                             b"helper refusal: path=/trusted/loader uid=0 gid=0 mode=0777 symlink=true\n"])
        with self.assertRaisesRegex(RuntimeError, "helper exited.*(?s:.*path=/trusted/loader uid=0)"):
            harness.wait_for_helper("failed.service")
        self.assertEqual(harness.run.call_count, 2)

    def test_missing_tree_is_already_removed_but_permission_errors_propagate(self):
        with tempfile.TemporaryDirectory() as tmp:
            q.remove_owned_tree(Path(tmp) / "missing")
        def denied(path, onerror):
            error = PermissionError("denied")
            onerror(None, path, (PermissionError, error, None))
        with mock.patch.object(q.shutil, "rmtree", side_effect=denied), self.assertRaises(PermissionError):
            q.remove_owned_tree(Path("/not-used"))

    def test_teardown_twice_tolerates_missing_helper_and_owned_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            registry, bindings, socket = root / "registry", root / "bindings.json", root / "helper.sock"
            registry.mkdir()
            mountinfo, forwarding = root / "mountinfo", root / "forwarding"
            mountinfo.write_text("1 0 8:1 / / rw - xfs /dev/root rw\n")
            forwarding.write_text("1\n")
            harness = object.__new__(q.Harness)
            harness.args = SimpleNamespace(tap_index=254)
            harness.name = "owned-fixture"
            harness.results = root / "results"
            harness.results.mkdir()
            harness.work, harness.home, harness.image = root / "work", root / "tenants/owned-fixture", root / "image"
            harness.registry = registry / "owned-fixture.json"
            harness.registry.write_text("{}\n")
            for path in (harness.work, harness.home, harness.image):
                path.mkdir(parents=True)
            harness.units, harness.processes = ["gone.service"], []
            harness.started_helper = harness.owned = True
            harness.bindings_before = None
            harness.base_jails_before = set()
            harness.forwarding_before = "0\n"
            harness.baseline = dict(failed_units=[])
            harness.record = mock.Mock()
            harness.helper = mock.Mock()
            harness.run = mock.Mock(return_value=b"")
            harness.console = mock.Mock(return_value="retained console\n")
            external = "motor-job-unrelated.service loaded failed failed unrelated fleet job"
            harness.host_state = mock.Mock(return_value=dict(tenant_vmm=[], tenant_taps=[], tenant_table=False,
                                                            netns=[], failed_units=[external], ipv4_forwarding="0"))
            with mock.patch.multiple(q, ROOT=root, REGISTRY=registry, BINDINGS=bindings, HELPER_SOCKET=socket,
                                     MOUNTINFO=mountinfo, FORWARDING=forwarding):
                harness.teardown()
                harness.teardown()
            harness.helper.assert_not_called()
            self.assertFalse(harness.registry.exists())
            self.assertFalse(harness.work.exists())
            self.assertEqual(forwarding.read_text(), "0\n")
            harness.run.assert_any_call("systemctl", "stop", "gone.service", allow_missing_unit=True)
            harness.record.assert_any_call("unrelated_new_failed_units", units=[external])
            self.assertFalse(any(call.args[:3] == ("systemctl", "reset-failed", "motor-job-unrelated.service") for call in harness.run.call_args_list))

    def test_failed_unit_ownership_uses_exact_names_or_tenant_components(self):
        units, tenant = ["owned-helper.service"], "tenant-id"
        self.assertTrue(q.owned_unit("owned-helper.service", "", units, tenant))
        self.assertTrue(q.owned_unit("child.service", "/system.slice/tenants/tenant-id/child.service", units, tenant))
        self.assertFalse(q.owned_unit("owned-helper.service-other", "/system.slice/other", units, tenant))
        self.assertFalse(q.owned_unit("tenant-id-unrelated.service", "/system.slice/tenant-id-other/child.service", units, tenant))

    def test_host_journal_archive_preserves_state_but_excludes_credentials(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            private = root / "host-private"
            journal = private / "journal"
            (journal / "outbox-expired").mkdir(parents=True)
            (journal / "journal.json").write_text('{"counter":42,"vms":{}}\n')
            (journal / "journal.new").write_text("transient crash evidence")
            (journal / "outbox-expired/1-0.json").write_text('{"job":"id"}\n')
            (private / "seed").write_text("secret seed")
            (journal / "not-state.key").write_text("not archived")
            output = root / "results/host-state"
            copied = q.archive_journal(journal, output)
            assert copied is not None
            self.assertEqual(set(copied), {"journal.json", "journal.new", "outbox-expired/1-0.json"})
            self.assertEqual((output / "journal.json").read_text(), '{"counter":42,"vms":{}}\n')
            self.assertFalse((output / "seed").exists())
            self.assertFalse((output / "not-state.key").exists())
            self.assertEqual((output / "journal.json").stat().st_mode & 0o777, 0o600)
            (journal / "journal.json").unlink()
            (journal / "journal.json").symlink_to(private / "seed")
            with self.assertRaises(OSError):
                q.archive_journal(journal, root / "no-link-read")
            self.assertFalse((root / "no-link-read/journal.json").exists())

    def test_transient_helper_waits_for_the_real_host_private_storage_unit(self):
        command = q.helper_unit_command("qualification.service", Path("/bin/helper"), Path("/config.json"))
        self.assertIn("--property=Requires=motor-vm-storage-private.service", command)
        self.assertIn("--property=After=motor-vm-storage-private.service", command)
        self.assertIn("--property=DelegateSubgroup=helper", command)
        self.assertEqual(command[-3:], [Path("/bin/helper"), "--config", Path("/config.json")])

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
        runner = next(value.split(":", 1)[1] for value in args.split() if value.startswith("systemd.set_credential_binary=systemd.unit-dropin.motor-tenant-runner.service:"))
        self.assertIn("StandardError=journal+console", base64.b64decode(runner).decode())

    def test_guest_runner_diagnostics_include_unit_journal_and_actual_socket_peer(self):
        diagnostics = q.module("guest_runner_diagnostics", q.HERE / "guest/hosted_qualify.py")
        def output(args, **_kwargs):
            text = "MainPID=123\nActiveState=active\n" if args[0] == "systemctl" else "actual daemon error canary\n"
            return subprocess.CompletedProcess(args, 0, stdout=text, stderr="")
        with mock.patch.object(diagnostics.socket, "SO_PEERCRED", 17, create=True), mock.patch.object(diagnostics.subprocess, "run", side_effect=output), mock.patch.object(diagnostics.Path, "lstat", return_value=SimpleNamespace(st_mode=0o140600, st_uid=0, st_ino=9)), mock.patch.object(diagnostics.socket, "socket") as socket_factory, mock.patch("builtins.print") as printed:
            socket_factory.return_value.__enter__.return_value.getsockopt.return_value = struct.pack("3i", 123, 0, 0)
            diagnostics.runner_diagnostics()
        line = printed.call_args.args[0]
        self.assertTrue(line.startswith("GUEST_RUNNER_DIAGNOSTIC "))
        state = json.loads(line.split(" ", 1)[1])
        self.assertIn("MainPID=123", state["unit"]["stdout"])
        self.assertIn("actual daemon error canary", state["journal"]["stdout"])
        self.assertEqual(state["socket"]["peer"]["pid"], 123)

    def test_guest_name_service_line_matches_the_host_expectation(self):
        guest = q.module("guest_name_service", q.HERE / "guest/hosted_qualify.py")
        self.assertEqual(guest.NAME_SERVICE, q.NAME_SERVICE)
        self.assertEqual(guest.TOOL_TREES, q.TOOL_TREES)
        self.assertEqual(guest.ROOT_PATHS, q.ROOT_PATHS)
        self.assertLess(q.STEPS.index("tool trees"), q.STEPS.index("root paths"))
        self.assertLess(q.STEPS.index("root paths"), q.STEPS.index("vsock isolation"))
        provision = __import__("inspect").getsource(q.Harness.provision)
        self.assertIn('shutil.copyfile(HERE / "guest/root_paths.py", dest / "root_paths.py")', provision)
        self.assertLess(q.STEPS.index("name service"), q.STEPS.index("tool trees"))
        self.assertLess(q.STEPS.index("tool trees"), q.STEPS.index("vsock isolation"))
        self.assertIn("name service", q.STEPS)
        self.assertLess(q.STEPS.index("tenant inventory"), q.STEPS.index("name service"))
        self.assertLess(q.STEPS.index("name service"), q.STEPS.index("vsock isolation"))

    def test_guest_tool_tree_probe_fails_on_copy_up_before_its_own_sample(self):
        guest = q.module("guest_tool_trees", q.HERE / "guest/hosted_qualify.py")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "rustup"
            (root / "lib").mkdir(parents=True)
            (root / "lib/librustc.so").write_text("library\n")
            upper = Path(directory) / "upper" / root.relative_to("/")
            upper.mkdir(parents=True)
            real_path = guest.Path
            def path(*parts):
                if parts == ("/overlay/upper",):
                    return real_path(directory, "upper")
                return real_path(*parts)
            commands = []
            with mock.patch.object(guest, "TOOL_ROOTS", (root,)), mock.patch.object(guest, "Path", side_effect=path), \
                    mock.patch.object(guest, "upper_bytes", return_value=0), \
                    mock.patch.object(guest, "tree_inventory", return_value=(3, 8, [])), \
                    mock.patch.object(guest.subprocess, "run", side_effect=lambda args, **_: commands.append(args)), \
                    mock.patch("builtins.print") as printed:
                with self.assertRaisesRegex(RuntimeError, "copied image tool trees"):
                    guest.tool_tree_probe()
            evidence = json.loads(printed.call_args_list[0].args[0].split(" ", 1)[1])
            self.assertEqual(evidence["copied_up_at_start"], [str(upper)])
            self.assertEqual(commands[0][:5], ["setfacl", "-P", "-m", "u:motor-job:rX", "--"])

    def test_guest_nameserver_parsing_ignores_comments_and_options(self):
        guest = q.module("guest_nameservers", q.HERE / "guest/hosted_qualify.py")
        self.assertEqual(guest.nameservers(""), [])
        self.assertEqual(guest.nameservers("# nameserver 9.9.9.9\noptions timeout:1\n"), [])
        self.assertEqual(guest.nameservers("nameserver 1.1.1.1\nnameserver 9.9.9.9\n"), ["1.1.1.1", "9.9.9.9"])

    def test_guest_runner_readiness_converts_monotonic_start_to_wall_clock(self):
        guest = q.module("guest_readiness", q.HERE / "guest/hosted_qualify.py")
        # Wall clock is 1000 s ahead of the monotonic clock; the runner started
        # at monotonic 50 s (wall 1050 s) and the marker was written at 1052.5 s.
        self.assertAlmostEqual(guest.runner_ready_seconds(50_000_000, 1052.5, 1100.0, 100.0), 2.5)
        with self.assertRaises(ValueError):
            guest.runner_ready_seconds(0, 1052.5, 1100.0, 100.0)

    def test_guest_name_service_probe_fails_on_a_slow_sudo_and_restores_resolv_conf(self):
        guest = q.module("guest_slow_sudo", q.HERE / "guest/hosted_qualify.py")
        files = {"/etc/resolv.conf": "", "/etc/hosts": "127.0.0.1\tlocalhost\n127.0.1.1\ttenant-a\n"}
        def read_text(path):
            return files[str(path)]
        def write_text(path, text):
            files[str(path)] = text
        def timed(*args):
            self.assertEqual(files["/etc/resolv.conf"], "nameserver 1.1.1.1\n")
            return (0, "", 12.0 if args[0] == "sudo" else 0.01)
        with mock.patch.object(guest.Path, "read_text", read_text), \
                mock.patch.object(guest.Path, "write_text", write_text), \
                mock.patch.object(guest.socket, "gethostname", return_value="tenant-a"), \
                mock.patch.object(guest, "timed", side_effect=timed), mock.patch("builtins.print"):
            with self.assertRaisesRegex(RuntimeError, "sudo -n true took 12.000 s"):
                guest.name_service_probe()
        self.assertEqual(files["/etc/resolv.conf"], "")

    def test_guest_name_service_probe_refuses_a_vm_nameserver_or_unmapped_host_name(self):
        guest = q.module("guest_vm_nameserver", q.HERE / "guest/hosted_qualify.py")
        for files, message in (
                ({"/etc/resolv.conf": "nameserver 1.1.1.1\n", "/etc/hosts": "127.0.1.1\ttenant-a\n"}, "names resolvers"),
                ({"/etc/resolv.conf": "", "/etc/hosts": "127.0.1.1\tmotor-tenant\n"}, "does not map")):
            with mock.patch.object(guest.Path, "read_text", lambda path, files=files: files[str(path)]), \
                    mock.patch.object(guest.socket, "gethostname", return_value="tenant-a"), \
                    mock.patch.object(guest, "timed", side_effect=AssertionError("probe ran")), \
                    mock.patch("builtins.print"):
                with self.assertRaisesRegex(RuntimeError, message):
                    guest.name_service_probe()

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

    def test_job_exchange_reads_past_heartbeats_before_every_reply(self):
        harness = q.Harness.__new__(q.Harness)
        harness.name = "qualify-host"
        harness.record = mock.Mock()
        harness.sign = lambda signing_input: b"signature"
        heartbeat = {"type": "heartbeat"}
        terminal = {"type": "hosted_terminal", "outcome": {"type": "exit", "code": 0}}
        # The host itself sends the challenge and lease_ok; runner heartbeats
        # can only follow, once the relay to the runner is open.
        replies = [{"type": "challenge", "host_id": "qualify-host", "nonce": "n", "ts": 1}, {"type": "lease_ok"},
                   heartbeat, heartbeat, {"type": "hello", "protocol_version": 1},
                   heartbeat, {"type": "accepted"},
                   heartbeat, {"type": "output", "stream": "stdout", "bytes": ""}, heartbeat, terminal]
        client, runner = socket.socketpair()
        with client, runner:
            for item in replies:
                q.send_frame(runner, item)
            frames = harness.converse(client, "job", {}, {"type": "run"}, b"bundle")
            client.shutdown(socket.SHUT_WR)
            sent = []
            while True:
                try:
                    sent.append(q.frame(runner)["type"])
                except EOFError:
                    break
        self.assertEqual(frames, [{"type": "output", "stream": "stdout", "bytes": ""}, terminal])
        self.assertEqual(sent, ["challenge_response", "hello", "run", "bundle_chunk"])

    def test_reply_wait_is_bounded_when_only_heartbeats_arrive(self):
        reads = []
        def heartbeat(_conn):
            reads.append(1)
            return {"type": "heartbeat"}
        with self.assertRaisesRegex(TimeoutError, "only heartbeats"):
            q.reply_frame(None, q.time.monotonic() + 0.05, read=heartbeat)
        self.assertGreater(len(reads), 0)
        self.assertEqual(q.reply_frame(None, q.time.monotonic() + 1, read=lambda _: {"type": "hello"}),
                         {"type": "hello"})

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
