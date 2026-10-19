#!/usr/bin/env python3
"""Portable build-plumbing tests; these do not claim to boot a VM or compile a kernel."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


image = module("tenant_image", HERE / "build.py")
capture = module("capture_versions", HERE / "capture-versions.py")
live = module("image_check", HERE / "guest/hosted_image_check.py")
guest = module("install_guest", HERE / "install-guest.py")


def resolved_config():
    return "\n".join(f"CONFIG_{line}" for line in (HERE / "kernel-options.txt").read_text().splitlines()
                     if line.endswith("=y")) + "\n"


class ImageTests(unittest.TestCase):
    def test_default_build_output_uses_root_trusted_vm_bind_view(self):
        with mock.patch.object(sys, "argv", ["build.py", "build", "test-version", "--runner", "/runner"]), mock.patch.object(image, "build") as build:
            image.main()
        self.assertEqual(build.call_args.args[1], Path("/var/lib/motor-vm/tenant-images").resolve())

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.versions = self.root / "versions.txt"
        self.versions.write_bytes((HERE / "versions.txt").read_bytes())
        self.runner = self.root / "runner"
        self.runner.write_bytes(b"guest runner input")
        self.calls = []

    def builder(self, stage, pins, runner, jobs):
        self.calls.append((pins, runner, jobs))
        image.stage_inputs(stage, pins, runner)
        (stage / "kernel").mkdir()
        (stage / "kernel/kernel.config").write_text(resolved_config())
        (stage / "kernel/vmlinux").write_bytes(b"kernel")
        (stage / "rootfs").write_bytes(b"root filesystem")
        (stage / "packages.txt").write_text("test-package\t1.0\n")
        (stage / "tools.json").write_text('{"fixture": true}\n')

    def build(self, version="test-1"):
        return image.build(version, self.root / "images", self.versions, self.runner, builder=self.builder)

    def change_pin(self, key, value):
        pins = json.loads(self.versions.read_text())
        pins[key] = value
        self.versions.write_text(json.dumps(pins))

    def test_pins_cover_real_runner_inputs(self):
        pins = image.load_versions(self.versions)
        for name in image.RUNNER_INPUTS:
            self.assertEqual(hashlib.sha256((image.REPO / name).read_bytes()).hexdigest(),
                             pins["runner_inputs_sha256"][name], name)
        self.assertEqual(pins["pnpm"], "11.1.2")
        self.assertIn("python3", pins["packages"])
        self.assertIn("systemd-sysv", pins["packages"])

    def test_unpinned_inputs_are_rejected(self):
        for key, value in (("node", "latest"), ("pnpm", "11.2.0"), ("rust_default", "stable"),
                           ("debian_snapshot", "latest"), ("rootfs_gib", 0), ("packages", ["bad; package"]),
                           ("runner_inputs_sha256", {}), ("kernel_sha256", "bad")):
            with self.subTest(key=key):
                self.versions.write_bytes((HERE / "versions.txt").read_bytes())
                self.change_pin(key, value)
                with self.assertRaises(ValueError):
                    image.load_versions(self.versions)

    def test_duplicate_pin_keys_are_rejected(self):
        self.versions.write_text('{"schema":1,"schema":1}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            image.load_versions(self.versions)

    def test_source_digest_is_checked_before_copying(self):
        source, dest = self.root / "source", self.root / "destination"
        source.write_text("drifted installer")
        with self.assertRaisesRegex(ValueError, "pinned input changed"):
            image.checked_copy(source, dest, "0" * 64)
        self.assertFalse(dest.exists())

    def test_manifest_hashes_actual_artifacts_and_exact_versions(self):
        target = self.build()
        manifest = json.loads((target / "manifest.json").read_text())
        self.assertEqual(manifest["versions"], json.loads(self.versions.read_text()))
        self.assertEqual(manifest["version"], "test-1")
        self.assertEqual(manifest["kernel_sha256"], hashlib.sha256(b"kernel").hexdigest())
        self.assertEqual(manifest["rootfs_sha256"], hashlib.sha256(b"root filesystem").hexdigest())
        self.assertEqual(manifest["runner_sha256"], hashlib.sha256(b"guest runner input").hexdigest())
        self.assertIn("root=/dev/vda ro init=/sbin/motor-init", manifest["boot_args"])
        self.assertFalse((target / ".work").exists())
        self.assertEqual((target / "rootfs").stat().st_mode & 0o222, 0)
        image.inventory(target, self.versions)

    def test_existing_version_refused_without_touching_artifacts(self):
        target = self.build()
        before = {path.name: path.read_bytes() for path in target.iterdir()}
        result = subprocess.run([sys.executable, str(HERE / "build.py"), "build", "test-1",
                                 "--output-root", str(target.parent), "--runner", "/not/a/runner",
                                 "--versions", "/not/a/versions"], text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FileExistsError", result.stderr)
        self.assertEqual({path.name: path.read_bytes() for path in target.iterdir()}, before)

    def test_rebuild_new_version_uses_same_pins(self):
        first, second = self.build(), self.build("test-2")
        for path in (first, second):
            image.inventory(path, self.versions)
        self.assertEqual(len(self.calls), 2)
        self.assertEqual(self.calls[0][0], self.calls[1][0])

    def test_inventory_rejects_corrupt_kernel(self):
        target = self.build()
        (target / "vmlinux").chmod(0o644)
        (target / "vmlinux").write_bytes(b"wrong kernel")
        with self.assertRaisesRegex(ValueError, "vmlinux SHA-256"):
            image.inventory(target, self.versions)

    def test_inventory_rejects_corrupt_rootfs(self):
        target = self.build()
        (target / "rootfs").chmod(0o644)
        (target / "rootfs").write_bytes(b"wrong rootfs")
        with self.assertRaisesRegex(ValueError, "rootfs SHA-256"):
            image.inventory(target, self.versions)

    def test_inventory_rejects_changed_versions(self):
        target = self.build()
        self.change_pin("node", "24.20.0")
        with self.assertRaisesRegex(ValueError, "differs from versions.txt"):
            image.inventory(target, self.versions)

    def test_failed_build_remains_reserved_and_cannot_pass_inventory(self):
        def fail(*_args):
            raise RuntimeError("stopped before unmount")

        with self.assertRaises(RuntimeError):
            image.build("failed", self.root / "images", self.versions, self.runner, builder=fail)
        target = self.root / "images/failed"
        with self.assertRaisesRegex(ValueError, "incomplete"):
            image.inventory(target, self.versions)
        with self.assertRaises(FileExistsError):
            self.build("failed")

    def test_version_cannot_escape_output_root(self):
        for name in ("", ".", "..", "a/b", "/absolute", "bad\nname"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.build(name)

    def test_kernel_options_include_boot_and_isolation_drivers(self):
        config = set(resolved_config().splitlines())
        for name in ("XFS_FS", "OVERLAY_FS", "VSOCKETS", "VIRTIO_VSOCKETS", "VIRTIO_BLK",
                     "EXT4_FS", "TMPFS", "MEMCG", "CGROUP_PIDS", "BPF_SYSCALL", "CGROUP_BPF",
                     "SECCOMP_FILTER", "NET_NS", "NF_TABLES", "VETH"):
            self.assertIn(f"CONFIG_{name}=y", config)

    def test_resolved_kernel_rejects_missing_driver_module_or_fallback_tunnel(self):
        path = self.root / "kernel.config"
        for name in ("XFS_FS", "OVERLAY_FS", "VIRTIO_VSOCKETS", "VIRTIO_BLK"):
            for replacement in ("m", "n"):
                with self.subTest(name=name, replacement=replacement):
                    path.write_text(resolved_config().replace(f"CONFIG_{name}=y", f"CONFIG_{name}={replacement}"))
                    with self.assertRaisesRegex(ValueError, f"CONFIG_{name} must be built in"):
                        image.check_kernel(path)
        path.write_text(resolved_config() + "CONFIG_NET_IPIP=m\n")
        with self.assertRaisesRegex(ValueError, "NET_IPIP must be disabled"):
            image.check_kernel(path)

    def test_resolved_kernel_rejects_modular_or_missing_nft_reject_stack(self):
        path = self.root / "kernel.config"
        for name in ("NFT_REJECT", "NFT_REJECT_INET", "NFT_REJECT_IPV4", "NFT_REJECT_IPV6",
                     "NF_REJECT_IPV4", "NF_REJECT_IPV6", "IPV6", "NETFILTER_NETLINK", "NF_TABLES_IPV4",
                     "NF_TABLES_IPV6", "NF_NAT_MASQUERADE", "NF_DEFRAG_IPV4", "NF_DEFRAG_IPV6"):
            for value in ("m", "n"):
                with self.subTest(name=name, value=value):
                    path.write_text(resolved_config().replace(f"CONFIG_{name}=y", f"CONFIG_{name}={value}"))
                    with self.assertRaisesRegex(ValueError, f"CONFIG_{name} must be built in"):
                        image.check_kernel(path)

    def test_module_load_requests_are_removed_without_masking_the_loader(self):
        directories = ("etc/modules-load.d", "run/modules-load.d", "usr/local/lib/modules-load.d",
                       "usr/lib/modules-load.d", "lib/modules-load.d")
        for relative in directories:
            directory = self.root / relative
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "vendor.conf").write_text("loop\nfuse\n")
            (directory / "empty.conf").write_text("")
        legacy = self.root / "etc/modules"
        legacy.write_text("loop\n")
        target = self.root / "preserve.txt"
        target.write_text("not a module config\n")
        (self.root / "etc/modules-load.d/link.conf").symlink_to(target)
        guest.clear_module_load_requests(self.root)
        self.assertEqual(guest.verify_no_module_load_requests(self.root), [])
        for relative in directories:
            self.assertEqual(list((self.root / relative).iterdir()), [])
        self.assertEqual(legacy.read_text(), "")
        self.assertEqual(target.read_text(), "not a module config\n")
        unit = self.root / "etc/systemd/system/systemd-modules-load.service"
        self.assertFalse(unit.exists())
        self.assertFalse(unit.is_symlink())
        dropin = self.root / "etc/systemd/system/systemd-modules-load.service.d/10-motor-console.conf"
        self.assertIn("StandardError=journal+console", dropin.read_text())

    def test_even_empty_modules_load_config_fails_image_verification(self):
        directory = self.root / "etc/modules-load.d"
        directory.mkdir(parents=True)
        (directory / "empty.conf").write_text("")
        with self.assertRaisesRegex(ValueError, "must not request loadable"):
            guest.verify_no_module_load_requests(self.root)

    def test_failed_motor_units_forward_their_own_output_to_console(self):
        for name in ("motor-net-rules.service", "motor-tenant-agent.service"):
            unit = (HERE / "guest" / name).read_text().splitlines()
            self.assertIn("StandardOutput=journal+console", unit)
            self.assertIn("StandardError=journal+console", unit)

    def test_capability_probe_rejects_missing_support_or_unexpected_success(self):
        for code, error in ((0, "tenant-agent takes no arguments"), (1, "unknown operation")):
            result = subprocess.CompletedProcess([], code, stderr=error)
            with mock.patch.object(guest.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(ValueError, "capability probe failed"):
                    guest.refuse_probe(["tenant-agent", "extra"], "tenant-agent takes no arguments")

    def test_capability_probe_recognizes_parse_time_refusal(self):
        result = subprocess.CompletedProcess([], 1, stderr="Error: tenant-agent takes no arguments")
        with mock.patch.object(guest.subprocess, "run", return_value=result):
            guest.refuse_probe(["tenant-agent", "extra"], "tenant-agent takes no arguments")

    def test_agent_uses_immutable_tools_and_carries_runner_environment(self):
        unit = (HERE / "guest/motor-tenant-agent.service").read_text()
        self.assertIn("Environment=CARGO_HOME=/usr/local/lib/motor/cargo\n", unit)
        self.assertIn("Environment=RUSTUP_HOME=/usr/local/lib/motor/rustup\n", unit)
        self.assertIn("Environment=HOME=/tenant\n", unit)
        self.assertIn("ExecStart=/usr/local/bin/motor-runner tenant-agent\n", unit)

    def test_capture_resolves_stable_alias_to_release_and_captures_all_targets(self):
        def output(*args):
            if args[:2] == ("rustup", "toolchain"):
                return "stable-x86_64-unknown-linux-gnu\n1.99.0-x86_64-unknown-linux-gnu (default)"
            if args[0] == "rustc":
                return "rustc 1.99.0 (test 2026-09-28)"
            if args[:2] == ("rustup", "target"):
                return "x86_64-unknown-linux-gnu\nwasm32-wasip1"
            return {"node": "v24.21.0", "npm": "11.19.0", "bun": "1.4.2", "pnpm": "11.1.2"}[args[0]]

        with mock.patch.object(capture, "output", side_effect=output):
            pins = capture.capture()
        self.assertEqual(pins["rust_toolchains"], {"1.99.0": ["wasm32-wasip1", "x86_64-unknown-linux-gnu"]})
        self.assertEqual(pins["rust_default"], "1.99.0")
        self.assertEqual(pins["node"], "24.21.0")

    def test_download_rejects_wrong_bytes(self):
        destination = self.root / "archive"

        def retrieve(_url, path):
            path.write_bytes(b"tampered")

        with mock.patch.object(guest.urllib.request, "urlretrieve", side_effect=retrieve):
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                guest.download("https://example.invalid/archive", destination, "0" * 64)

    def test_mountinfo_parser_separates_filesystem_from_mount_options(self):
        table = live.mounts("19 1 0:3 / / rw,relatime - overlay overlay rw,lowerdir=/\n"
                            "20 19 8:0 / /.lower ro,relatime - ext4 /dev/vda ro\n")
        self.assertEqual(table["/"][0], "overlay")
        self.assertIn("ro", table["/.lower"][1])

    def rootfs_shell(self, fail_chroot=False):
        stage, bins = self.root / "stage", self.root / "bin"
        stage.mkdir()
        bins.mkdir()
        (stage / "versions.json").write_bytes(self.versions.read_bytes())
        (stage / "script").mkdir()
        (stage / "guest").mkdir()
        (stage / "motor-runner").write_bytes(b"test runner")
        log = self.root / "commands.jsonl"
        stub = '''import json, os, pathlib, sys
name, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
log = pathlib.Path(os.environ["STUB_LOG"])
with log.open("a") as stream:
    stream.write(json.dumps([name, *args]) + "\\n")
if name == "id": print("0")
elif name == "uname": print("Linux" if args == ["-s"] else "x86_64")
elif name == "mountpoint": sys.exit(0)
elif name == "chroot":
    if os.environ.get("FAIL_CHROOT"): sys.exit(7)
    root = pathlib.Path(args[0])
    (root / "srv/build/packages.txt").write_text("test-package 1.0\\n")
    (root / "srv/build/tools.json").write_text("{}\\n")
elif name in ("truncate", "mkfs.ext4"):
    pathlib.Path(args[-1]).write_bytes(b"test rootfs")
'''
        for name in ("id", "uname", "debootstrap", "mountpoint", "mount", "umount", "chroot", "truncate", "mkfs.ext4"):
            path = bins / name
            path.write_text(f"#!{sys.executable}\n" + stub)
            path.chmod(0o755)
        env = dict(os.environ, PATH=f"{bins}:/usr/bin:/bin", STUB_LOG=str(log), JOBS="2")
        if fail_chroot:
            env["FAIL_CHROOT"] = "1"
        result = subprocess.run(["bash", str(HERE / "build-rootfs.sh"), str(stage)],
                                env=env, text=True, capture_output=True)
        return result, [json.loads(line) for line in log.read_text().splitlines()], stage

    def test_rootfs_shell_clears_host_env_and_unmounts_before_packing(self):
        result, calls, stage = self.rootfs_shell()
        self.assertEqual(result.returncode, 0, result.stderr)
        chroot = next(call for call in calls if call[0] == "chroot")
        self.assertEqual(chroot[2:4], ["/usr/bin/env", "-i"])
        self.assertIn("RUSTC_WRAPPER=", chroot)
        self.assertIn("CARGO_BUILD_RUSTC_WRAPPER=", chroot)
        unmounts = [i for i, call in enumerate(calls) if call[0] == "umount"]
        packing = next(i for i, call in enumerate(calls) if call[0] == "mkfs.ext4")
        self.assertEqual(len(unmounts), 4)
        self.assertLess(max(unmounts), packing)
        self.assertEqual((stage / "rootfs").read_bytes(), b"test rootfs")

    def test_rootfs_shell_failure_unmounts_but_does_not_publish_image(self):
        result, calls, stage = self.rootfs_shell(fail_chroot=True)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(len([call for call in calls if call[0] == "umount"]), 4)
        self.assertFalse(any(call[0] == "mkfs.ext4" for call in calls))
        self.assertFalse((stage / "rootfs").exists())


if __name__ == "__main__":
    print(f"Python {sys.version.split()[0]}: portable tenant-image tests", flush=True)
    unittest.main(verbosity=2)
