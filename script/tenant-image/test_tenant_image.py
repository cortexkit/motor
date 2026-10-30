#!/usr/bin/env python3
"""Portable build-plumbing tests; these do not claim to boot a VM or compile a kernel."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
ROOT_MOUNTINFO = "1 0 8:1 / / rw - xfs /dev/root rw\n"


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
                     if line.endswith("=y")) + '\nCONFIG_LSM="landlock,lockdown,yama,integrity,apparmor"\n'


class ImageTests(unittest.TestCase):
    def test_ipv6_is_loopback_only_before_the_tenant_agent_can_start(self):
        self.assertNotIn('ipv6.disable=', image.BOOT_ARGS)
        rules = (HERE / 'guest/motor-net-rules.service').read_text().splitlines()
        setup = ('ExecStartPre=/usr/sbin/sysctl -q -w '
                 'net.core.devconf_inherit_init_net=0 '
                 'net.ipv6.conf.all.disable_ipv6=1 net.ipv6.conf.default.disable_ipv6=1 '
                 'net.ipv6.conf.lo.disable_ipv6=0')
        self.assertIn(setup, rules)
        self.assertLess(rules.index(setup), rules.index('ExecStart=/usr/sbin/nft -f /etc/nftables.d/motor-net.nft'))
        agent = (HERE / 'guest/motor-tenant-agent.service').read_text().splitlines()
        self.assertIn('After=local-fs.target motor-net-rules.service', agent)
        self.assertIn('Requires=motor-net-rules.service', agent)

    def test_configure_uses_tenants_pinned_job_identity(self):
        class StopAfterIdentity(Exception):
            pass
        account = mock.Mock()
        account.ensure.side_effect = StopAfterIdentity
        spec = mock.Mock()
        with mock.patch.object(guest, 'BUILD', HERE.parents[1]), \
             mock.patch.object(guest.importlib.util, 'spec_from_file_location', return_value=spec), \
             mock.patch.object(guest.importlib.util, 'module_from_spec', return_value=account):
            with self.assertRaises(StopAfterIdentity):
                guest.configure({})
        account.ensure.assert_called_once_with({'uid': 999, 'gid': 986}, guest.run)

    def test_default_build_output_uses_root_trusted_vm_bind_view(self):
        with mock.patch.object(sys, "argv", ["build.py", "build", "test-version", "--runner", "/runner", "--build-budget-gib", "1"]), mock.patch.object(image, "build") as build:
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
        return image.build(version, self.root / "images", self.versions, self.runner, builder=self.builder,
                           build_budget=1024**3, free_floor=0, mountinfo_reader=lambda: ROOT_MOUNTINFO)

    def change_pin(self, key, value):
        pins = json.loads(self.versions.read_text())
        pins[key] = value
        self.versions.write_text(json.dumps(pins))

    def test_new_image_advertises_sparse_and_resize_capabilities(self):
        target = self.build()
        manifest = json.loads((target / "manifest.json").read_text())
        self.assertEqual(["sparse-v1", "resize-v1"], manifest["capabilities"])

    def test_pins_cover_real_runner_inputs(self):
        pins = image.load_versions(self.versions)
        for name in image.RUNNER_INPUTS:
            self.assertEqual(hashlib.sha256((image.REPO / name).read_bytes()).hexdigest(),
                             pins["runner_inputs_sha256"][name], name)
        self.assertEqual(pins["pnpm"], "11.1.2")
        self.assertIn("python3", pins["packages"])
        self.assertIn("systemd-sysv", pins["packages"])

    def test_debootstrap_includes_the_pinned_git_build_dependencies(self):
        pins = image.load_versions(self.versions)
        manifest = json.loads((image.REPO / "script/runner/toolchain.json").read_text())
        missing = set(manifest["tools"]["git"]["source"]["build_packages"]) - set(pins["packages"])
        self.assertEqual(set(), missing)
        self.assertIn("script/runner/build-git.sh", image.RUNNER_INPUTS)
        installer = (HERE / "install-guest.py").read_text()
        self.assertIn('shared.tools(manifest, role="tenant")', installer)
        shared = (image.REPO / 'script/boat-image/install.py').read_text()
        self.assertIn('run("bash", str(ROOT / "script/runner/build-git.sh"))', shared)

    def test_guest_git_must_be_the_pinned_build_first_on_the_agent_path(self):
        manifest = json.loads((image.REPO / "script/runner/toolchain.json").read_text())
        unit = HERE / "guest/motor-tenant-agent.service"
        searched = []

        def which(name, path):
            searched.append(path)
            return "/usr/local/bin/git"
        with mock.patch.object(guest, "run", return_value="git version 2.53.0") as run:
            self.assertEqual("git version 2.53.0", guest.verify_git(manifest, unit, which))
        run.assert_called_once_with("/usr/local/bin/git", "--version", capture=True)
        self.assertEqual(["/usr/local/lib/motor/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"], searched)
        with mock.patch.object(guest, "run", return_value="git version 2.47.3"):
            with self.assertRaisesRegex(ValueError, "git version differs"):
                guest.verify_git(manifest, unit, which)
        with mock.patch.object(guest, "run", return_value="git version 2.53.0"):
            with self.assertRaisesRegex(ValueError, "not the pinned build"):
                guest.verify_git(manifest, unit, lambda name, path: "/usr/bin/git")

    def test_binaryen_120_is_installed_from_the_pinned_archive_and_verified(self):
        pins = image.load_versions(self.versions)
        manifest = json.loads((image.REPO / "script/runner/toolchain.json").read_text())
        spec = manifest['tools']['wasm-opt']
        self.assertNotIn('binaryen', pins['packages'])
        self.assertIn('script/runner/install-binaryen.py', image.RUNNER_INPUTS)
        installer = (HERE / 'install-guest.py').read_text()
        self.assertIn('run("apt-get", "purge", "-y", "binaryen")', installer)
        shared = (image.REPO / 'script/boat-image/install.py').read_text()
        self.assertIn('script/runner/install-binaryen.py', shared)
        self.assertEqual('/usr/local/bin/wasm-opt', spec['path_by_role']['tenant'])
        self.assertEqual('wasm-opt version 120', spec['version'])
        observed = 'wasm-opt version 120 (version_120)'
        unit = HERE / 'guest/motor-tenant-agent.service'
        which = lambda name, path: '/usr/local/bin/wasm-opt'
        with mock.patch.object(guest, 'run', return_value=observed) as run:
            self.assertEqual(observed, guest.verify_wasm_opt(manifest, unit, which))
        run.assert_called_once_with('/usr/local/bin/wasm-opt', '--version', capture=True)
        with mock.patch.object(guest, 'run', return_value='wasm-opt version 108'):
            with self.assertRaisesRegex(ValueError, 'wasm-opt major version differs'):
                guest.verify_wasm_opt(manifest, unit, which)
        with self.assertRaisesRegex(ValueError, 'not the pinned Binaryen release'):
            guest.verify_wasm_opt(manifest, unit, which=lambda name, path: '/usr/bin/wasm-opt')

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

    def test_kernel_must_implement_descriptor_relative_chmod(self):
        for version in ('6.1.128', '6.6.0', '6.12.114'):
            with self.subTest(version=version):
                self.change_pin('kernel', version)
                with self.assertRaisesRegex(ValueError, 'Firecracker-supported.*fchmodat2'):
                    image.load_versions(self.versions)
        self.change_pin('kernel', '6.18.0')
        self.assertEqual('6.18.0', image.load_versions(self.versions)['kernel'])

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
                                 "--build-budget-gib", "1",
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

    def test_verify_accepts_an_installed_image_built_from_older_pins(self):
        # A rollback switches back to an image built before today's pins
        # changed; its own recorded pins and hashes are what must hold.
        target = self.build()
        self.change_pin("node", "24.20.0")
        self.assertEqual(image.verify(target)["version"], "test-1")

    def test_verify_rejects_tampering_and_self_inconsistency(self):
        target = self.build()
        (target / "rootfs").chmod(0o644)
        (target / "rootfs").write_bytes(b"wrong rootfs")
        with self.assertRaisesRegex(ValueError, "rootfs SHA-256"):
            image.verify(target)
        target = self.build("test-2")
        self.change_pin("node", "24.20.0")
        (target / "versions.txt").chmod(0o644)
        (target / "versions.txt").write_text(self.versions.read_text())
        with self.assertRaisesRegex(ValueError, "differs from its manifest"):
            image.verify(target)

    def test_failed_build_remains_reserved_and_cannot_pass_inventory(self):
        def fail(*_args):
            raise RuntimeError("stopped before unmount")

        with self.assertRaises(RuntimeError):
            image.build("failed", self.root / "images", self.versions, self.runner, builder=fail,
                        build_budget=1024**3, free_floor=0, mountinfo_reader=lambda: ROOT_MOUNTINFO)
        target = self.root / "images/failed"
        with self.assertRaisesRegex(ValueError, "incomplete"):
            image.inventory(target, self.versions)
        with self.assertRaises(FileExistsError):
            self.build("failed")

    def test_mounted_stage_is_retained_without_recursive_delete_or_manifest(self):
        ready = False
        def builder(stage, pins, runner, jobs):
            nonlocal ready
            self.builder(stage, pins, runner, jobs)
            (stage / "root/dev").mkdir(parents=True)
            ready = True
        stage = self.root / "images/blocked/.work"
        def mountinfo():
            return ROOT_MOUNTINFO + (f"2 1 0:2 / {stage / 'root/dev'} rw - tmpfs tmpfs rw\n" if ready else "")
        caught = None
        with mock.patch.object(image.shutil, "rmtree") as remove:
            try:
                image.build("blocked", self.root / "images", self.versions, self.runner, builder=builder,
                            build_budget=1024**3, free_floor=0, mountinfo_reader=mountinfo)
            except Exception as error:
                caught = error
            remove.assert_not_called()
        self.assertIsInstance(caught, RuntimeError)
        self.assertRegex(str(caught), "mounted paths:.*root/dev")
        self.assertTrue((stage / "rootfs").is_file())
        self.assertFalse((stage.parent / "manifest.json").exists())
        usage = json.loads((stage.parent / "disk-usage.json").read_text())
        self.assertEqual(usage["outcome"], "failed")

    def test_real_rootfs_builder_uses_a_private_mount_namespace(self):
        stage = self.root / "private-stage"
        stage.mkdir()
        execute = mock.Mock()
        with mock.patch.object(image.platform, "system", return_value="Linux"), mock.patch.object(image.platform, "machine", return_value="x86_64"), mock.patch.object(image.os, "geteuid", return_value=0):
            image.build_artifacts(stage, image.load_versions(self.versions), self.runner, 4, run=execute)
        self.assertEqual(execute.call_args_list[1].args[0][:6], ["unshare", "--mount", "--propagation", "private", "--", "bash"])

    def test_storage_preflight_refuses_before_builder(self):
        builder = mock.Mock()
        with mock.patch.object(image.shutil, "disk_usage", return_value=type("Usage", (), dict(free=3 * 1024**3 - 1))):
            with self.assertRaisesRegex(ValueError, "floor.*budget"):
                image.build("space", self.root / "images", self.versions, self.runner, builder=builder,
                            build_budget=1024**3, free_floor=2 * 1024**3, mountinfo_reader=lambda: ROOT_MOUNTINFO)
        builder.assert_not_called()
        self.assertFalse((self.root / "images/space/manifest.json").exists())

    def test_cleanup_command_refuses_a_nonstage_root_even_for_root(self):
        with mock.patch.object(sys, "argv", ["build.py", "cleanup-mounts", "/"]), mock.patch.object(sys, "stderr"), mock.patch.object(image.os, "geteuid", return_value=0), mock.patch.object(image, "unmount_tree") as unmount:
            with self.assertRaises(SystemExit) as error:
                image.main()
        self.assertEqual(error.exception.code, 2)
        unmount.assert_not_called()

    def test_internal_subcommands_do_not_require_a_build_budget(self):
        config = self.root / "kernel.config"
        config.write_text(resolved_config())
        with mock.patch.object(sys, "argv", ["build.py", "check-kernel", str(config)]):
            image.main()
        root = self.root / ".work/root"
        with mock.patch.object(sys, "argv", ["build.py", "cleanup-mounts", str(root)]), mock.patch.object(image.os, "geteuid", return_value=0), mock.patch.object(image, "unmount_tree") as unmount:
            image.main()
        unmount.assert_called_once_with(root.resolve())
        with mock.patch.object(sys, "argv", ["build.py", "check-mounts", str(root)]), mock.patch.object(image, "assert_unmounted") as fence:
            image.main()
        fence.assert_called_once_with(root.resolve())

    def test_measured_profile_admits_same_recipe_and_refuses_changed_pins(self):
        with mock.patch.object(image.shutil, "disk_usage", return_value=type("Usage", (), dict(free=1024**4))):
            first = self.build("first")
            profile = first / "disk-usage.json"
            second = image.build("second", first.parent, self.versions, self.runner, builder=self.builder,
                                 peak_profile=profile, free_floor=0, mountinfo_reader=lambda: ROOT_MOUNTINFO)
            self.assertEqual(json.loads((second / "disk-usage.json").read_text())["recipe_digest"], json.loads(profile.read_text())["recipe_digest"])
            self.change_pin("node", "24.20.0")
            with self.assertRaisesRegex(ValueError, "profile.*match"):
                image.build("different", first.parent, self.versions, self.runner, builder=self.builder,
                            peak_profile=profile, free_floor=0, mountinfo_reader=lambda: ROOT_MOUNTINFO)
        self.assertEqual(len(self.calls), 2)

    def test_version_cannot_escape_output_root(self):
        for name in ("", ".", "..", "a/b", "/absolute", "bad\nname"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.build(name)

    def test_kernel_options_include_boot_and_isolation_drivers(self):
        config = set(resolved_config().splitlines())
        for name in ("XFS_FS", "OVERLAY_FS", "VSOCKETS", "VIRTIO_VSOCKETS", "VIRTIO_BLK",
                     "VIRTIO_VSOCKETS_COMMON", "VIRTIO", "VIRTIO_MMIO", "VIRTIO_MMIO_CMDLINE_DEVICES",
                     "VIRTIO_NET", "VIRTIO_CONSOLE", "SERIAL_8250", "SERIAL_8250_CONSOLE", "PRINTK",
                     "HW_RANDOM", "HW_RANDOM_VIRTIO",
                     "SYSFS", "MULTIUSER", "SECURITY", "SECURITY_PATH", "SECURITY_NETWORK", "SECURITY_LANDLOCK",
                     "EXT4_FS", "TMPFS", "MEMCG", "CGROUP_PIDS", "BPF_SYSCALL", "CGROUP_BPF",
                     "SECCOMP_FILTER", "NET_NS", "NF_TABLES", "VETH"):
            self.assertIn(f"CONFIG_{name}=y", config)

    def test_resolved_kernel_rejects_missing_driver_module_or_fallback_tunnel(self):
        path = self.root / "kernel.config"
        for name in ("XFS_FS", "OVERLAY_FS", "VIRTIO_VSOCKETS", "VIRTIO_BLK",
                     "VIRTIO_VSOCKETS_COMMON", "HW_RANDOM", "HW_RANDOM_VIRTIO", "PRINTK"):
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

    def test_resolved_kernel_rejects_disabled_or_modular_landlock_dependencies(self):
        path = self.root / "kernel.config"
        for name in ("SYSFS", "MULTIUSER", "SECURITY", "SECURITY_PATH", "SECURITY_NETWORK", "SECURITY_LANDLOCK"):
            for replacement in (f"# CONFIG_{name} is not set", f"CONFIG_{name}=m", ""):
                with self.subTest(name=name, replacement=replacement):
                    path.write_text(resolved_config().replace(f"CONFIG_{name}=y", replacement))
                    with self.assertRaisesRegex(ValueError, f"CONFIG_{name} must be built in"):
                        image.check_kernel(path)

    def test_resolved_kernel_requires_psi_enabled_at_boot(self):
        # A job's cgroup memory.pressure file exists only with PSI on; the job
        # isolation contract promises it to jobs.
        path = self.root / "kernel.config"
        for config in (resolved_config().replace("CONFIG_PSI=y\n", "# CONFIG_PSI is not set\n"),
                       resolved_config().replace("CONFIG_PSI=y\n", "CONFIG_PSI=m\n")):
            with self.subTest(config=config[-60:]):
                path.write_text(config)
                with self.assertRaisesRegex(ValueError, "CONFIG_PSI must be built in"):
                    image.check_kernel(path)
        path.write_text(resolved_config() + "CONFIG_PSI_DEFAULT_DISABLED=y\n")
        with self.assertRaisesRegex(ValueError, "CONFIG_PSI_DEFAULT_DISABLED must be disabled"):
            image.check_kernel(path)

    def test_resolved_kernel_rejects_disabled_landlock(self):
        path = self.root / "kernel.config"
        config = resolved_config().replace("CONFIG_SECURITY_LANDLOCK=y\n", "")
        path.write_text(config + "# CONFIG_SECURITY_LANDLOCK is not set\n")
        with self.assertRaisesRegex(ValueError, "CONFIG_SECURITY_LANDLOCK must be built in"):
            image.check_kernel(path)

    def test_resolved_kernel_requires_landlock_in_lsm_list_without_replacing_other_lsms(self):
        path = self.root / "kernel.config"
        original = 'CONFIG_LSM="landlock,lockdown,yama,integrity,apparmor"'
        for value in ('"landlock,lockdown,yama,integrity,apparmor"',
                      '"lockdown,yama,landlock,integrity,apparmor"'):
            with self.subTest(value=value):
                path.write_text(resolved_config().replace(original, f"CONFIG_LSM={value}"))
                image.check_kernel(path)
        for replacement in ('CONFIG_LSM="lockdown,yama,integrity,apparmor"',
                            'CONFIG_LSM="notlandlock"', ""):
            with self.subTest(replacement=replacement):
                path.write_text(resolved_config().replace(original, replacement))
                with self.assertRaisesRegex(ValueError, "CONFIG_LSM must include landlock"):
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

    def test_tool_trees_are_published_world_readable_and_verified_without_following_links(self):
        toolchain = self.root / "rustup"
        (toolchain / "lib").mkdir(parents=True)
        (toolchain / "lib/librustc.so").write_text("library\n")
        (toolchain / "rustc").write_text("rustc\n")
        (toolchain / "rustc").chmod(0o750)
        (toolchain / "lib/librustc.so").chmod(0o640)
        (toolchain / "lib").chmod(0o770)
        outside = self.root / "private"
        outside.write_text("secret\n")
        outside.chmod(0o600)
        (toolchain / "link").symlink_to(outside)
        # BSD-style systems give new files their directory's group.
        owner, group = os.getuid(), toolchain.lstat().st_gid
        with self.assertRaisesRegex(ValueError, "not plainly world-readable"):
            guest.verify_tool_trees([toolchain], acl=lambda _: False, owner=owner, group=group)
        own = mock.Mock()
        guest.publish_tool_trees([toolchain], own=own)
        self.assertIn(mock.call(toolchain / "link", 0, 0), own.call_args_list)
        self.assertEqual((toolchain / "rustc").stat().st_mode & 0o7777, 0o755)
        self.assertEqual((toolchain / "lib/librustc.so").stat().st_mode & 0o7777, 0o644)
        self.assertEqual((toolchain / "lib").stat().st_mode & 0o7777, 0o755)
        self.assertEqual(outside.stat().st_mode & 0o7777, 0o600)
        self.assertEqual(guest.verify_tool_trees([toolchain], acl=lambda _: False, owner=owner, group=group),
                         {str(toolchain): 4})
        with self.assertRaisesRegex(ValueError, "not plainly world-readable"):
            guest.verify_tool_trees([toolchain], acl=lambda path: path.name == "rustc", owner=owner, group=group)
        with self.assertRaisesRegex(ValueError, "not root-owned"):
            guest.verify_tool_trees([toolchain], acl=lambda _: False, owner=owner + 1, group=group)

    def test_agent_directory_normalization_and_verification_share_all_components(self):
        relatives = ("tenant", "Users", "home", "usr/local/lib/motor/cargo/bin")
        for relative in relatives:
            (self.root / relative).mkdir(parents=True, exist_ok=True)
        components = ("", "tenant", "Users", "home", "usr", "usr/local", "usr/local/lib",
                      "usr/local/lib/motor", "usr/local/lib/motor/cargo", "usr/local/lib/motor/cargo/bin")
        for relative in components:
            (self.root / relative).chmod(0o2775)
        with self.assertRaisesRegex(ValueError, "unsafe image agent directory"):
            guest.verify_agent_paths(self.root, owner=os.getuid())
        own = mock.Mock()
        guest.normalize_agent_paths(self.root, own=own)
        self.assertEqual(guest.verify_agent_paths(self.root, owner=os.getuid()),
                         ["/tenant", "/Users", "/home", "/usr/local/lib/motor/cargo/bin"])
        for relative in components:
            path = self.root / relative
            own.assert_any_call(path, 0, 0)
            self.assertEqual(path.stat().st_mode & 0o7777, 0o755)
            path.chmod(0o2775)
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "unsafe image agent directory"):
                guest.verify_agent_paths(self.root, owner=os.getuid())
            path.chmod(0o755)

    def test_agent_image_path_checks_do_not_follow_symlinks_or_accept_wrong_owner(self):
        for path in ("tenant", "Users", "home", "usr/local/lib/motor/cargo/bin"):
            (self.root / path).mkdir(parents=True, exist_ok=True)
        guest.normalize_agent_paths(self.root, own=mock.Mock())
        with self.assertRaisesRegex(ValueError, "expected uid="):
            guest.verify_agent_paths(self.root, owner=os.getuid() + 1)
        library = self.root / "usr/local/lib"
        import shutil
        shutil.rmtree(library)
        target = self.root / "elsewhere"
        target.mkdir()
        target.chmod(0o777)
        library.symlink_to(target, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "not a real directory"):
            guest.normalize_agent_paths(self.root, own=mock.Mock())
        with self.assertRaisesRegex(ValueError, "unsafe image agent directory"):
            guest.verify_agent_paths(self.root, owner=os.getuid())
        self.assertEqual(target.stat().st_mode & 0o7777, 0o777)

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

    def test_image_build_runs_the_tenant_runners_own_javascript_setup(self):
        scratch = self.root / "javascript-setup-check"
        calls = []

        def run(*args, capture=False):
            calls.append(args)
            scratch.mkdir()
            return "bun/bun.lock: /usr/local/lib/motor/bun/bin/bun 1.4.2\n" + guest.JAVASCRIPT_SETUP_PASS
        with mock.patch.object(guest, "run", side_effect=run):
            lines = guest.verify_javascript_setup(scratch)
        # --tenant-vm selects the same tool locations the serving tenant runner uses.
        self.assertEqual([("motor-runner", "check-javascript-setup", "--tenant-vm", "--scratch", str(scratch))], calls)
        self.assertEqual(guest.JAVASCRIPT_SETUP_PASS, lines[-1])
        self.assertFalse(scratch.exists())
        with mock.patch.object(guest, "run", side_effect=lambda *a, **k: scratch.mkdir() or "no pass line"):
            with self.assertRaisesRegex(ValueError, "did not pass"):
                guest.verify_javascript_setup(scratch)
        import inspect
        self.assertIn("verify_javascript_setup()", inspect.getsource(guest.verify))

    def test_nothing_the_image_installs_stays_under_the_bound_workspace_roots(self):
        home = self.root / "home/ubuntu"
        (home / ".bun/bin").mkdir(parents=True)
        with self.assertRaisesRegex(ValueError, r"hides at boot: \[.*\.bun"):
            guest.retire_tool_home(home)
        (home / ".bun/bin").rmdir()
        (home / ".bun").rmdir()
        guest.retire_tool_home(home)
        self.assertFalse(home.exists())
        users = self.root / "Users"
        users.mkdir()
        self.assertEqual([str(self.root / "home"), str(users)],
                         guest.verify_workspace_roots_empty((self.root / "home", users)))
        (users / "leftover").write_text("")
        with self.assertRaisesRegex(ValueError, "binds over at attach"):
            guest.verify_workspace_roots_empty((self.root / "home", users))

    def test_agent_uses_immutable_tools_and_carries_runner_environment(self):
        unit = (HERE / "guest/motor-tenant-agent.service").read_text()
        self.assertIn("Environment=CARGO_HOME=/usr/local/lib/motor/cargo\n", unit)
        self.assertIn("Environment=RUSTUP_HOME=/usr/local/lib/motor/rustup\n", unit)
        # The runner finds its JavaScript tools at its role's fixed paths, not
        # through HOME, and must not read configuration from beneath /home:
        # in a tenant VM that is the tenant's own bound directory.
        self.assertIn("Environment=HOME=/tenant\n", unit)
        self.assertNotIn("/home", unit)
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
        stage, bins = self.root / ".work", self.root / "bin"
        stage.mkdir()
        bins.mkdir()
        (stage / "versions.json").write_bytes(self.versions.read_bytes())
        (stage / "script").mkdir()
        (stage / "script/runner").mkdir()
        (stage / "script/runner/toolchain.json").write_bytes((image.REPO / "script/runner/toolchain.json").read_bytes())
        (stage / "guest").mkdir()
        (stage / "motor-runner").write_bytes(b"test runner")
        log = self.root / "commands.jsonl"
        mounts = self.root / "mounts.json"
        mounts.write_text("[]")
        stub = '''import json, os, pathlib, sys
name, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
log = pathlib.Path(os.environ["STUB_LOG"])
mounts = pathlib.Path(os.environ["STUB_MOUNTS"])
with log.open("a") as stream:
    stream.write(json.dumps([name, *args]) + "\\n")
if name == "id": print("0")
elif name == "uname": print("Linux" if args == ["-s"] else "x86_64")
elif name == "mount" and not any(a.startswith("--make-") for a in args):
    paths = json.loads(mounts.read_text()) + [args[-1]]
    if "--rbind" in args:
        paths += [args[-1] + "/pts", args[-1] + "/shm", args[-1] + "/shm/deep"]
    mounts.write_text(json.dumps(paths))
elif name == "umount":
    paths = json.loads(mounts.read_text())
    paths.remove(args[-1])
    mounts.write_text(json.dumps(paths))
elif name == "python3":
    if len(args) >= 3 and args[1] == "cleanup-mounts":
        import importlib.util
        spec = importlib.util.spec_from_file_location("storage", pathlib.Path(args[0]).with_name("build_storage.py"))
        storage = importlib.util.module_from_spec(spec); spec.loader.exec_module(storage)
        def mountinfo():
            return "1 0 8:1 / / rw - xfs /dev/root rw\\n" + "".join(
                f"{i+2} 1 0:2 / {p} rw - tmpfs stub rw\\n" for i,p in enumerate(json.loads(mounts.read_text())))
        storage.unmount_tree(pathlib.Path(args[2]), reader=mountinfo)
    else:
        os.execv(sys.executable, [sys.executable, *args])
elif name == "chroot":
    if os.environ.get("FAIL_CHROOT"): sys.exit(7)
    root = pathlib.Path(args[0])
    (root / "srv/build/packages.txt").write_text("test-package 1.0\\n")
    (root / "srv/build/tools.json").write_text("{}\\n")
elif name in ("truncate", "mkfs.ext4"):
    pathlib.Path(args[-1]).write_bytes(b"test rootfs")
'''
        for name in ("id", "uname", "debootstrap", "mount", "umount", "chroot", "truncate", "mkfs.ext4", "python3"):
            path = bins / name
            path.write_text(f"#!{sys.executable}\n" + stub)
            path.chmod(0o755)
        env = dict(os.environ, PATH=f"{bins}:/usr/bin:/bin", STUB_LOG=str(log), STUB_MOUNTS=str(mounts), JOBS="2")
        if fail_chroot:
            env["FAIL_CHROOT"] = "1"
        result = subprocess.run(["bash", str(HERE / "build-rootfs.sh"), str(stage)],
                                env=env, text=True, capture_output=True)
        return result, [json.loads(line) for line in log.read_text().splitlines()], stage

    def test_rootfs_shell_clears_host_env_and_unmounts_before_packing(self):
        result, calls, stage = self.rootfs_shell()
        self.assertEqual(result.returncode, 0, result.stderr)
        debootstrap = next(call for call in calls if call[0] == 'debootstrap')
        included = next(arg.removeprefix('--include=') for arg in debootstrap if arg.startswith('--include=')).split(',')
        manifest = json.loads((image.REPO / 'script/runner/toolchain.json').read_text())
        self.assertIn('openssh-server', included)
        self.assertTrue(set(manifest['apt_packages']['required']).issubset(included))
        self.assertTrue(set(manifest['apt_packages']['by_role']['tenant']).issubset(included))
        chroot = next(call for call in calls if call[0] == "chroot")
        self.assertEqual(chroot[2:4], ["/usr/bin/env", "-i"])
        self.assertIn("RUSTC_WRAPPER=", chroot)
        self.assertIn("CARGO_BUILD_RUSTC_WRAPPER=", chroot)
        unmounts = [i for i, call in enumerate(calls) if call[0] == "umount"]
        packing = next(i for i, call in enumerate(calls) if call[0] == "mkfs.ext4")
        self.assertEqual(len(unmounts), 7)
        paths = [call[-1] for call in calls if call[0] == "umount"]
        self.assertLess(paths.index(str(stage / "root/dev/shm/deep")), paths.index(str(stage / "root/dev/shm")))
        self.assertLess(paths.index(str(stage / "root/dev/shm")), paths.index(str(stage / "root/dev")))
        self.assertLess(max(unmounts), packing)
        self.assertEqual((stage / "rootfs").read_bytes(), b"test rootfs")

    def test_rootfs_shell_failure_unmounts_but_does_not_publish_image(self):
        result, calls, stage = self.rootfs_shell(fail_chroot=True)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(len([call for call in calls if call[0] == "umount"]), 7)
        self.assertFalse(any(call[0] == "mkfs.ext4" for call in calls))
        self.assertFalse((stage / "rootfs").exists())


root_paths = guest.root_paths
qualification = module("hosted_qualify", HERE / "guest/hosted_qualify.py")
discard = module("discard_fixture", HERE / "discard_fixture.py")
BOAT = HERE.parent / "boat-image"
INSTALL_TOOLS = HERE.parent / "runner/install-tools.sh"


class DiscardTests(unittest.TestCase):
    def test_isolated_discard_host_threshold_is_at_least_900_mib(self):
        before = 2 * 1024**3
        self.assertEqual(900 * 1024**2, discard.assert_reclaimed(before, before - 900 * 1024**2))
        for after in [before, before - 900 * 1024**2 + 1, before + 1024]:
            with self.assertRaisesRegex(RuntimeError, "discard reclaimed"):
                discard.assert_reclaimed(before, after)

    def test_isolated_discard_measures_host_allocated_blocks_not_file_length(self):
        with tempfile.TemporaryDirectory() as directory:
            disk = Path(directory) / "disk.img"
            with disk.open("wb") as file:
                file.truncate(600 * 1024**3)
            self.assertEqual(disk.stat().st_blocks * 512, discard.allocated(disk))
            self.assertLess(discard.allocated(disk), disk.stat().st_size)

    def test_isolated_discard_guest_cold_and_warm_use_measurement_barriers(self):
        for phase in ("cold", "warm"):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                events = []

                def run(argv, **kwargs):
                    events.append(argv)
                    self.assertTrue(kwargs["check"])
                    if argv[0] == "dd":
                        self.assertIn("count=1024", argv)
                        self.assertIn("bs=1048576", argv)
                        self.assertIn("conv=fsync", argv)
                        (root / "qualification-discard.bin").write_bytes(b"allocated fixture")
                    if argv[0] == "fstrim":
                        self.assertFalse((root / "qualification-discard.bin").exists())

                discard.guest_cycle(phase, tenant=root, workspace=root, run=run,
                                    wait=lambda path: events.append(path.name),
                                    emit=lambda line, **kw: events.append(line))
                expected = (["discard-warm-start"] if phase == "warm" else []) + [
                    ["dd", "if=/dev/zero", f"of={root / 'qualification-discard.bin'}", "bs=1048576", "count=1024", "conv=fsync"],
                    f"DISCARD {phase} allocated", f"discard-{phase}-measured",
                    ["sync", "-f", str(root)], ["fstrim", "--verbose", str(root)], f"DISCARD {phase} trimmed"]
                self.assertEqual(expected, events)

    def test_isolated_discard_control_changes_only_tenant_discard(self):
        requests = [("/drives/tenant", dict(drive_id="tenant", discard=True, io_engine="Sync")),
                    ("/drives/rootfs", dict(drive_id="rootfs", is_read_only=True))]
        controlled = discard.without_discard(requests)
        self.assertEqual(False, controlled[0][1]["discard"])
        self.assertEqual("Sync", controlled[0][1]["io_engine"])
        self.assertEqual(requests[1], controlled[1])
        self.assertTrue(requests[0][1]["discard"])


def unit_paths(unit):
    """The PATH directories and RUSTUP_HOME a systemd unit file sets."""
    paths = []
    for line in unit.read_text().splitlines():
        if line.startswith("Environment=PATH="):
            paths += line.removeprefix("Environment=PATH=").split(":")
        if line.startswith("Environment=RUSTUP_HOME="):
            paths.append(line.removeprefix("Environment=RUSTUP_HOME="))
    return paths


# The account that owns fixtures the tests mark as foreign. Node's release
# tarball records uid 1001, but GitHub's Ubuntu runners also run tests as uid
# 1001, so a foreign owner must differ from the test's own uid.
FOREIGN_UID = 1001 if os.getuid() != 1001 else 1002


class RootPathTests(unittest.TestCase):
    """The invariant that everything root executes from is root-owned with no
    group or other write. Fixtures are owned by the test's own uid, so `owner`
    is that uid; a directory another account owns is simulated through lstat
    unless the test runs as root, which can create one for real."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.image = Path(os.path.realpath(temp.name), "image")
        for relative in ("usr/local/bin", "usr/local/lib/motor/cargo/bin", "usr/local/include",
                         "usr/local/share", "usr/bin", "usr/sbin"):
            (self.image / relative).mkdir(parents=True)
        for relative in ("usr/local/bin/motor-runner", "usr/local/bin/node", "usr/bin/python3", "usr/sbin/init"):
            (self.image / relative).write_text("#!/bin/sh\n")
            (self.image / relative).chmod(0o755)
        (self.image / "usr/local/bin/corepack").symlink_to("node")
        # Merged /usr: /bin and /sbin are relative links into /usr.
        (self.image / "bin").symlink_to("usr/bin")
        (self.image / "sbin").symlink_to("usr/sbin")
        self.image.chmod(0o755)
        self.owner = os.getuid()
        self.foreign = set()

    def own_by_foreign(self, relative):
        path = self.image / relative
        if os.geteuid() == 0:
            os.lchown(path, FOREIGN_UID, FOREIGN_UID)
        else:
            self.foreign.add(path)

    def simulated_owners(self):
        real = Path.lstat

        def lstat(path):
            metadata = real(path)
            if path in self.foreign:
                return mock.Mock(st_uid=FOREIGN_UID, st_gid=FOREIGN_UID, st_mode=metadata.st_mode)
            return metadata

        return mock.patch.object(Path, "lstat", lstat)

    def check(self, roots=root_paths.TENANT_ROOTS):
        with self.simulated_owners():
            return root_paths.check(roots, self.image, self.owner)

    def test_clean_tree_passes_and_walks_merged_usr_once(self):
        report = self.check()
        self.assertEqual(report["offenders"], [])
        self.assertEqual(report["offender_count"], 0)
        # The image root, usr, the 11 entries under usr/local, usr/bin and its
        # file, usr/sbin and its file, and the /bin and /sbin links themselves.
        self.assertEqual(report["entries"], 19)

    def test_a_foreign_usr_local_bin_fails_naming_the_directory(self):
        # What extracting Node's release tarball as root used to leave behind.
        self.own_by_foreign("usr/local/bin")
        self.own_by_foreign("usr/local/bin/node")
        report = self.check()
        self.assertEqual(report["offender_count"], 2)
        self.assertEqual(report["offenders"][0], f"{self.image}/usr/local/bin: uid={FOREIGN_UID} gid={FOREIGN_UID}")
        with self.simulated_owners():
            with self.assertRaisesRegex(ValueError, rf"2 path\(s\) root executes from .*usr/local/bin: uid={FOREIGN_UID}"):
                root_paths.require(root_paths.TENANT_ROOTS, self.image, self.owner)

    def test_a_provider_restored_file_passes_only_at_its_exact_path_owner_and_mode(self):
        (self.image / "usr/local/sbin").mkdir()
        tool = self.image / "usr/local/sbin/ascii-idle-reclaim"
        tool.write_text("#!/bin/sh\n")
        tool.chmod(0o755)
        self.own_by_foreign("usr/local/sbin/ascii-idle-reclaim")
        allowed = {"/usr/local/sbin/ascii-idle-reclaim": FOREIGN_UID}

        def check(provider_files):
            with self.simulated_owners():
                return root_paths.check(root_paths.TENANT_ROOTS, self.image, self.owner,
                                        provider_files=provider_files)

        # Without the allowance the strict rule names it; with it, it passes.
        self.assertEqual(check(None)["offender_count"], 1)
        report = check(allowed)
        self.assertEqual(report["offenders"], [])
        self.assertEqual(report["provider_restored"], [f"{tool}: uid={FOREIGN_UID} (provider-restored)"])
        # The allowance is for that exact owner: another uid still fails.
        self.assertEqual(check({"/usr/local/sbin/ascii-idle-reclaim": FOREIGN_UID + 1})["offender_count"], 1)
        # And never for group or other write.
        tool.chmod(0o775)
        self.assertEqual(check(allowed)["offender_count"], 1)
        tool.chmod(0o755)
        # A different file the provider owns beside it still fails.
        other = self.image / "usr/local/sbin/other"
        other.write_text("#!/bin/sh\n")
        other.chmod(0o755)
        self.own_by_foreign("usr/local/sbin/other")
        self.assertEqual(check(allowed)["offender_count"], 1)
        # The live Boat probe names exactly this one path.
        self.assertEqual(root_paths.BOAT_PROVIDER_RESTORED, {"/usr/local/sbin/ascii-idle-reclaim": 1000})

    def test_a_foreign_symlink_fails_but_its_mode_bits_are_ignored(self):
        self.assertEqual(self.check()["offenders"], [])
        self.own_by_foreign("usr/local/bin/corepack")
        self.assertEqual(self.check()["offenders"], [f"{self.image}/usr/local/bin/corepack: uid={FOREIGN_UID} gid={FOREIGN_UID}"])

    def test_group_or_other_write_fails_on_entries_and_on_ancestors(self):
        for relative in ("usr/local/share", "usr/local/bin/node", "usr", ""):
            path = self.image / relative
            mode = path.lstat().st_mode & 0o7777
            for extra in (0o020, 0o002):
                path.chmod(mode | extra)
                with self.subTest(path=relative or "/", extra=oct(extra)):
                    self.assertEqual(self.check()["offenders"],
                                     [f"{path}: mode={mode | extra:04o} (group or other may write)"])
            path.chmod(mode)

    def test_link_targets_and_path_roots_outside_usr_local_are_checked(self):
        self.own_by_foreign("usr/sbin/init")
        self.assertEqual(self.check()["offenders"], [f"{self.image}/usr/sbin/init: uid={FOREIGN_UID} gid={FOREIGN_UID}"])

    def test_missing_roots_are_listed_and_escaping_links_fail(self):
        report = self.check()
        self.assertEqual((report["offenders"], report["missing"]), ([], []))
        (self.image / "usr/local/sbin").symlink_to("/")
        report = self.check(("/usr/local/sbin",))
        self.assertEqual(report["offenders"],
                         [f"{self.image}/usr/local/sbin: resolves outside the checked image to /"])

    def test_unreadable_directories_raise_instead_of_passing_unseen(self):
        if os.geteuid() == 0:
            self.skipTest("root reads every directory")
        closed = self.image / "usr/local/lib/motor"
        closed.chmod(0)
        self.addCleanup(closed.chmod, 0o755)
        with self.assertRaises(PermissionError):
            self.check()

    def test_roots_cover_every_directory_the_image_units_execute_from(self):
        def covered(path, roots):
            return any(path == root or path.startswith(root + "/") for root in roots)

        tenant = [path for name in ("motor-tenant-agent.service", "motor-qualification.service")
                  for path in unit_paths(HERE / "guest" / name)]
        boat = unit_paths(BOAT / "motor-boat.service")
        self.assertIn("/usr/local/lib/motor/cargo/bin", tenant)
        self.assertIn("/home/ubuntu/.rustup", boat)
        for name, paths, roots in (("tenant", tenant, root_paths.TENANT_ROOTS), ("boat", boat, root_paths.BOAT_ROOTS)):
            for path in paths:
                with self.subTest(image=name, path=path):
                    self.assertTrue(covered(path, roots))

    def test_tenant_units_run_nothing_from_the_bound_workspace_roots(self):
        # The tenant agent binds the tenant's /home and /Users over the image's
        # before the runner starts; a tool the image placed there vanishes.
        for name in ("motor-tenant-agent.service", "motor-qualification.service"):
            for path in unit_paths(HERE / "guest" / name):
                with self.subTest(unit=name, path=path):
                    self.assertFalse(any(path == root or path.startswith(root + "/")
                                         for root in ("/home", "/Users")))
        for root in root_paths.TENANT_ROOTS:
            self.assertFalse(root.startswith(("/home", "/Users")), root)

    def test_recipes_enforce_the_invariant_after_installation(self):
        import inspect
        verify = inspect.getsource(guest.verify)
        self.assertLess(verify.index("verify_tool_trees()"), verify.index("root_paths.require(root_paths.TENANT_ROOTS)"))
        self.assertLess(verify.index("root_paths.require(root_paths.TENANT_ROOTS)"), verify.index("tools.json"))
        boat = (BOAT / "install.py").read_text()
        main = boat[boat.index("def main():"):]
        self.assertLess(main.index("service_preflight()"), main.index("root_paths.require(root_paths.BOAT_ROOTS)"))
        self.assertLess(main.index("root_paths.require(root_paths.BOAT_ROOTS)"), main.index("inventory.json"))

    def test_every_archive_extraction_drops_recorded_owners(self):
        sources = {path: path.read_text() for path in (HERE / "install-guest.py", BOAT / "install.py", INSTALL_TOOLS)}
        extractions = 0
        for path, text in sources.items():
            for number, line in enumerate(text.splitlines(), 1):
                code = line.split("#")[0]
                # The tar command itself, not "target", "tarball" or "tar.xz".
                if re.search(r"(?<![\w.-])tar(?![\w.])", code):
                    extractions += 1
                    with self.subTest(path=path.name, line=number):
                        self.assertIn("--no-same-owner", line)
                        self.assertIn("--no-same-permissions", line)
                if "cp -a" in code:
                    with self.subTest(path=path.name, line=number):
                        self.assertIn("--no-preserve=ownership", line)
        # extract() in each Python recipe and the tar.xz branch of install-tools.sh.
        self.assertEqual(extractions, 3)
        for path in (HERE / "install-guest.py", BOAT / "install.py"):
            with self.subTest(path=path.name):
                self.assertNotIn('run("tar", "-x', sources[path])

    def test_extract_masks_recorded_modes_and_restores_the_umask(self):
        import io
        import tarfile
        archive = self.image.parent / "node.tar.gz"
        with tarfile.open(archive, "w:gz") as stream:
            for name, kind, mode in (("node-v1/bin", tarfile.DIRTYPE, 0o777), ("node-v1/bin/node", tarfile.REGTYPE, 0o777)):
                entry = tarfile.TarInfo(name)
                entry.type, entry.mode, entry.uid, entry.gid = kind, mode, 1001, 1001
                stream.addfile(entry, io.BytesIO(b"") if kind == tarfile.REGTYPE else None)
        destination = self.image.parent / "extracted"
        destination.mkdir()
        previous = os.umask(0)
        try:
            guest.extract(archive, destination, "-z", "--strip-components=1")
            self.assertEqual(os.umask(0), 0)
        finally:
            os.umask(previous)
        for relative in ("bin", "bin/node"):
            metadata = (destination / relative).lstat()
            with self.subTest(path=relative):
                self.assertEqual(metadata.st_mode & 0o7777, 0o755)
                self.assertEqual(metadata.st_uid, os.geteuid())

    def test_qualification_probe_fails_a_drifted_guest_and_passes_a_clean_one(self):
        with mock.patch("builtins.print") as printed:
            qualification.root_path_probe(self.image, self.owner)
        self.assertEqual(printed.call_args.args[0], qualification.ROOT_PATHS)
        self.image.joinpath("usr/local/include").chmod(0o775)
        with mock.patch("builtins.print") as printed, self.assertRaisesRegex(RuntimeError, "usr/local/include: mode=0775"):
            qualification.root_path_probe(self.image, self.owner)
        evidence = json.loads(printed.call_args_list[0].args[0].split(" ", 1)[1])
        self.assertEqual(evidence["offender_count"], 1)


if __name__ == "__main__":
    print(f"Python {sys.version.split()[0]}: portable tenant-image tests", flush=True)
    unittest.main(verbosity=2)
