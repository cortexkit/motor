#!/usr/bin/env python3
"""Portable checks for the job-image split; no mounts, image builds or VM boots."""

import hashlib
import json
import os
from pathlib import Path
import runpy
import socket
import stat
import struct
import subprocess
import tempfile
import unittest
from unittest import mock

import build as image
import job_images


class JobImageTests(unittest.TestCase):
    def test_pinned_hello_arguments_match_the_helpers_job_runner_arguments(self):
        # The image pins the Hello its runner prints with these arguments; the
        # booted runner gets the helper's. A drift would refuse every job.
        helper = Path(__file__).resolve().parents[2] / "crates/motor-vm-helper/src/linux/job.rs"
        source = " ".join(helper.read_text().split())
        backend = " ".join(job_images.JOB_RUNNER_HELLO_ARGS[:3])
        returns = " ".join(job_images.JOB_RUNNER_HELLO_ARGS[3:])
        self.assertIn(f'"{backend} --job-network', source)
        self.assertIn(returns, source)

    def test_missing_installer_loader_refuses_build(self):
        with mock.patch.object(job_images.importlib.util, "spec_from_file_location", return_value=None):
            with self.assertRaisesRegex(ImportError, "shared tenant installer"):
                job_images.installer()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.stage = Path(self.temp.name)
        self.root = self.stage / "root"
        self.tools = self.stage / "tools"
        for path in ("usr/local/lib/motor/rustup", "usr/local/lib/motor/cargo/bin",
                     "usr/local/lib/motor/bun/bin", "usr/local/lib/node_modules/npm/bin",
                     "usr/local/lib/node_modules/pnpm/bin", "usr/local/bin", "sbin"):
            (self.root / path).mkdir(parents=True, exist_ok=True)
        (self.root / "sbin/motor-init").write_bytes((job_images.HERE / "guest/motor-init").read_bytes())
        (self.root / "sbin/motor-init").chmod(0o755)
        for name in ("node", "motor-runner"):
            path = self.root / "usr/local/bin" / name
            path.write_bytes(b"executable")
            path.chmod(0o750)
        for name in ("npm", "pnpm"):
            target = self.root / f"usr/local/lib/node_modules/{name}/bin/{name}.js"
            target.write_bytes(b"javascript")
            target.chmod(0o700)
            (self.root / "usr/local/bin" / name).symlink_to(f"../lib/node_modules/{name}/bin/{name}.js")
        (self.root / "etc").mkdir()
        self.job_uid = os.geteuid() + 1
        (self.root / "etc/passwd").write_text(f"motor-job:x:{self.job_uid}:1001::/:/usr/sbin/nologin\n")
        (self.stage / "rootfs").write_bytes(b"already packed tenant ext4")

    def split(self):
        uid, gid = os.geteuid(), os.getegid()
        job_images.split_tree(self.root, self.tools,
                              own=lambda path, _uid, _gid: os.lchown(path, uid, gid),
                              owner=uid, group=gid)

    def test_split_preserves_tenant_bytes_guest_files_links_exec_and_empty_cargo_roots(self):
        tenant = (self.stage / "rootfs").read_bytes()
        passwd = (self.root / "etc/passwd").read_bytes()
        self.split()
        self.assertEqual((self.stage / "rootfs").read_bytes(), tenant)
        self.assertEqual((self.root / "etc/passwd").read_bytes(), passwd)
        self.assertEqual(list((self.root / "usr/local").iterdir()), [])
        local = self.tools / "usr/local"
        for name in job_images.REQUIRED:
            self.assertTrue((local / name).exists(), name)
        for name in ("registry", "git"):
            path = local / "lib/motor/cargo" / name
            self.assertEqual(list(path.iterdir()), [])
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o755)
        self.assertEqual(os.readlink(local / "bin/npm"), "../lib/node_modules/npm/bin/npm.js")
        self.assertEqual(stat.S_IMODE((local / "bin/node").stat().st_mode), 0o755)
        job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())
        self.assertIn('exec /sbin/motor-job-toolchain-init "$@"', (self.root / "sbin/motor-init").read_text())
        self.assertEqual((self.root / "sbin/motor-job-toolchain-init").read_bytes(),
                         (job_images.HERE / "guest/motor-job-toolchain-init").read_bytes())

    def test_split_refuses_missing_tool(self):
        (self.root / "usr/local/bin/node").unlink()
        with self.assertRaisesRegex(ValueError, "lacks /usr/local/bin/node"):
            self.split()

    def test_split_refuses_unrecognized_init(self):
        (self.root / "sbin/motor-init").write_text("not the init handoff")
        with self.assertRaisesRegex(ValueError, "unrecognized tenant init"):
            self.split()

    def test_split_clears_inherited_acls_before_provisioning_without_following_links(self):
        removed = set()
        def attributes(path, **_kwargs):
            return [name for name in ("system.posix_acl_access", "system.posix_acl_default")
                    if (path, name) not in removed]
        with mock.patch.object(os, "listxattr", side_effect=attributes), \
             mock.patch.object(os, "removexattr", side_effect=lambda path, name, **_kwargs: removed.add((path, name))) as remove:
            self.split()
        self.assertTrue(remove.call_count)
        self.assertEqual({call.args[1] for call in remove.call_args_list},
                         {"system.posix_acl_access", "system.posix_acl_default"})
        for call in remove.call_args_list:
            self.assertFalse(call.kwargs["follow_symlinks"])
            self.assertFalse(call.args[0].is_symlink())

    def test_verifier_refuses_setuid_setgid_and_sticky_bits_on_files_and_directories(self):
        self.split()
        for path in (self.tools / "usr/local/bin/node", self.tools / "usr/local/bin"):
            for bit in (stat.S_ISUID, stat.S_ISGID, stat.S_ISVTX):
                with self.subTest(path=path, bit=bit):
                    path.chmod(0o755 | bit)
                    with self.assertRaisesRegex(ValueError, "special toolchain permission"):
                        job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())
                    path.chmod(0o755)

    def test_verifier_refuses_fifo_and_socket_fixtures(self):
        self.split()
        fifo = self.tools / "fifo"
        os.mkfifo(fifo, 0o644)
        with self.assertRaisesRegex(ValueError, "unsupported toolchain file type"):
            job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())
        fifo.unlink()
        sock = self.tools / "socket"
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(sock))
            sock.chmod(0o644)
            with self.assertRaisesRegex(ValueError, "unsupported toolchain file type"):
                job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())

    def test_verifier_refuses_access_and_default_acl_fixtures(self):
        self.split()
        leaf = self.tools / "usr/local/bin/node"
        directory = leaf.parent
        original_getxattr = os.getxattr
        def access(path, name, **kwargs):
            if path == leaf and name == "system.posix_acl_access":
                return b"fixture access ACL"
            return original_getxattr(path, name, **kwargs)
        with mock.patch.object(os, "getxattr", side_effect=access), \
             self.assertRaisesRegex(ValueError, "plainly world-readable"):
            job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())
        with mock.patch.object(os, "listxattr", side_effect=lambda path, **_kwargs:
                               ["system.posix_acl_default"] if path == directory else []), \
             self.assertRaisesRegex(ValueError, "default toolchain ACL"):
            job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())

    def test_cargo_lower_acls_use_guest_uid_and_cover_every_descendant(self):
        self.job_uid = 54321
        (self.root / "etc/passwd").write_text(f"motor-job:x:{self.job_uid}:1001::/:/usr/sbin/nologin\n")
        self.split()
        for name in ("registry", "git"):
            tree = self.tools / "usr/local/lib/motor/cargo" / name
            nested = tree / "nested/deeper"
            nested.mkdir(parents=True)
            (nested / "source").write_text("read only\n")
            (nested / "source").chmod(0o644)
            (nested / "executable").write_text("tool\n")
            (nested / "executable").chmod(0o755)
            (nested / "link").symlink_to("source")
        walk = job_images.installer().tool_tree_entries
        job_images.provision_cargo_acls(self.tools, self.job_uid, walk)
        job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())
        undefined = 0xffffffff
        for name in ("registry", "git"):
            tree = self.tools / "usr/local/lib/motor/cargo" / name
            for path in walk(tree):
                if path.is_symlink():
                    continue
                raw = os.getxattr(path, job_images.ACL_ACCESS)
                self.assertEqual(raw[:4], b"\x02\x00\x00\x00")
                executable = path.is_dir() or path.name == "executable"
                permissions = 5 if executable else 4
                self.assertEqual(list(struct.iter_unpack("<HHI", raw[4:])),
                                 [(1, 7 if executable else 6, undefined), (2, permissions, self.job_uid),
                                  (4, permissions, undefined), (16, permissions, undefined),
                                  (32, permissions, undefined)])
                if path.is_dir():
                    default = os.getxattr(path, job_images.ACL_DEFAULT)
                    self.assertEqual(list(struct.iter_unpack("<HHI", default[4:])),
                                     [(1, 7, undefined), (2, 5, self.job_uid), (4, 5, undefined),
                                      (16, 5, undefined), (32, 5, undefined)])
        for name in ("rustup", "cargo/bin", "bun/bin"):
            path = self.tools / "usr/local/lib/motor" / name
            self.assertIsNone(job_images.read_acl(path, job_images.ACL_ACCESS))
            self.assertIsNone(job_images.read_acl(path, job_images.ACL_DEFAULT))

    def test_cargo_acl_exceptions_reject_missing_wrong_extra_and_writable_grants(self):
        self.split()
        tree = self.tools / "usr/local/lib/motor/cargo/registry"
        valid = {name: os.getxattr(tree, name) for name in (job_images.ACL_ACCESS, job_images.ACL_DEFAULT)}
        wrong_user = bytearray(valid[job_images.ACL_ACCESS])
        struct.pack_into("<I", wrong_user, 4 + 8 + 4, self.job_uid + 1)
        writable = bytearray(valid[job_images.ACL_ACCESS])
        struct.pack_into("<H", writable, 4 + 8 + 2, 7)
        extra = [(1, 7, 0xffffffff), (2, 5, self.job_uid), (2, 5, self.job_uid + 1),
                 (4, 5, 0xffffffff), (16, 5, 0xffffffff), (32, 5, 0xffffffff)]
        extra = struct.pack("<I", 2) + b"".join(struct.pack("<HHI", *entry) for entry in extra)
        for name, value in [(job_images.ACL_ACCESS, None), (job_images.ACL_DEFAULT, None),
                            (job_images.ACL_ACCESS, wrong_user), (job_images.ACL_ACCESS, writable),
                            (job_images.ACL_DEFAULT, extra)]:
            with self.subTest(name=name, value=value):
                for attr, raw in valid.items():
                    os.setxattr(tree, attr, raw)
                if value is None:
                    os.removexattr(tree, name)
                else:
                    os.setxattr(tree, name, bytes(value))
                with self.assertRaisesRegex(ValueError, "Cargo source requires exact"):
                    job_images.verify_toolchain(self.tools, self.job_uid, owner=os.geteuid(), group=os.getegid())

    def test_job_uid_requires_one_valid_nonroot_guest_account(self):
        for contents in ("", "motor-job:x:0:0::/:/bin/false\n", "motor-job:x:invalid:1::/:/bin/false\n",
                         "motor-job:x:4294967295:1::/:/bin/false\n",
                         "motor-job:x:1:1::/:/bin/false\n" * 2):
            (self.root / "etc/passwd").write_text(contents)
            with self.assertRaisesRegex(ValueError, "motor-job uid"):
                job_images.job_account_uid(self.root)

    def test_source_acl_roots_track_the_runner_layers(self):
        source = (image.REPO / "crates/motor-runner/src/isolation/cargo_sources.rs").read_text()
        self.assertIn('["registry", "git"].map', source)
        self.assertEqual(job_images.CARGO_SOURCES, ("registry", "git"))

    def test_job_manifest_matches_helper_schema_and_hashes_real_files(self):
        (self.stage / "kernel").mkdir()
        (self.stage / "kernel/vmlinux").write_bytes(b"pinned tenant kernel")
        (self.stage / "job").mkdir()
        (self.stage / "job/rootfs.ext4").write_bytes(b"job root")
        (self.stage / "job/toolchain.ext4").write_bytes(b"toolchain")
        target = self.stage / "staging/generation"
        target.mkdir(parents=True)
        pinned_hello = {"type": "hello", "protocol_version": 1, "return_paths": True}
        runner = self.stage / "motor-runner"
        # Like the real runner, refuse a return maximum given without a budget.
        runner.write_text("#!/bin/sh\ntest \"$1\" = print-hello || exit 42\n"
                          "case \"$*\" in *'--return-chunked-max-bytes 134217728 --return-budget-bytes 2147483648'*) ;; *) exit 43 ;; esac\n"
                          "printf '%s\\n' '" + json.dumps(pinned_hello) + "'\n")
        runner.chmod(0o755)
        manifest = job_images.publish(self.stage, target)
        self.assertFalse((target / "manifest.json").exists())
        job_images.complete(target, manifest)
        self.assertEqual(set(p.name for p in target.iterdir()),
                         {"kernel", "rootfs.ext4", "toolchain.ext4", "manifest.json"})
        self.assertEqual(set(manifest), {"kernel", "rootfs", "toolchain", "hello", "guest_init"})
        self.assertEqual(manifest["hello"], pinned_hello)
        self.assertEqual(manifest["guest_init"], {})
        for key, name in (("kernel", "kernel"), ("rootfs", "rootfs.ext4"), ("toolchain", "toolchain.ext4")):
            data = (target / name).read_bytes()
            self.assertEqual(manifest[key], dict(bytes=len(data), sha256=hashlib.sha256(data).hexdigest()))
            self.assertEqual(stat.S_IMODE((target / name).stat().st_mode), 0o444)
        self.assertEqual(json.loads((target / "manifest.json").read_text()), manifest)
        self.assertEqual((self.stage / "kernel/vmlinux").read_bytes(), b"pinned tenant kernel")

    def test_image_runner_hello_cannot_be_an_empty_placeholder(self):
        with mock.patch.object(job_images.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, stdout="{}")):
            with self.assertRaisesRegex(ValueError, "valid pinned Hello"):
                job_images.runner_hello(self.stage / "motor-runner")

    def test_empty_artifact_is_never_published(self):
        path = self.stage / "empty"
        path.touch()
        with self.assertRaisesRegex(ValueError, "empty job image artifact"):
            job_images.artifact(path)

    def test_build_packs_both_split_trees_with_the_pinned_gib_aligned_size(self):
        run = mock.Mock()
        with mock.patch.object(job_images, "split_tree") as split:
            job_images.build(self.stage, 32, run=run)
        split.assert_called_once_with(self.root, self.stage / "job-tools")
        self.assertEqual(run.call_args_list, [
            mock.call(["truncate", "-s", "32G", str(self.stage / "job/rootfs.ext4")], check=True),
            mock.call(["mkfs.ext4", "-F", "-L", "motor-job-root", "-d", str(self.root),
                       str(self.stage / "job/rootfs.ext4")], check=True),
            mock.call(["truncate", "-s", "32G", str(self.stage / "job/toolchain.ext4")], check=True),
            mock.call(["mkfs.ext4", "-F", "-L", "motor-toolchain", "-d", str(self.stage / "job-tools"),
                       str(self.stage / "job/toolchain.ext4")], check=True),
        ])

    def test_generation_refusal_precedes_output_reservation_or_building(self):
        for version, generation in (("1.2.3", None), ("valid", "pins"), ("valid", "locks"),
                                    ("valid", "../bad"), ("valid", "a.b")):
            with self.subTest(version=version, generation=generation):
                builder = mock.Mock()
                with self.assertRaisesRegex(ValueError, "--job-generation"):
                    image.build(version, self.stage / "images", self.stage / "versions", self.stage / "runner",
                                builder=builder, job_generation=generation)
                builder.assert_not_called()
                self.assertFalse((self.stage / "images").exists())

    def test_bootstrap_mounts_readonly_before_init_and_each_mount_failure_stops_boot(self):
        script = str(job_images.HERE / "guest/motor-job-toolchain-init")
        expected = [
            ["/usr/bin/mount", "-t", "ext4", "-o", "ro,nosuid,nodev,dax=always", "/dev/pmem0", "/toolchain"],
            ["/usr/bin/mount", "--bind", "/toolchain/usr/local", "/usr/local"],
            ["/usr/bin/mount", "-o", "remount,bind,ro,nosuid,nodev", "/usr/local"],
        ]
        with mock.patch.object(Path, "mkdir"), mock.patch.object(subprocess, "run") as run, mock.patch.object(os, "execv") as execute:
            runpy.run_path(script)
        self.assertEqual(run.call_args_list, [mock.call(command, check=True) for command in expected])
        execute.assert_called_once_with("/sbin/init", ["/sbin/init"])
        for index in range(3):
            with mock.patch.object(Path, "mkdir"), mock.patch.object(os, "execv") as execute:
                def run(command, check):
                    self.assertTrue(check)
                    if command == expected[index]:
                        raise subprocess.CalledProcessError(1, command)
                with mock.patch.object(subprocess, "run", side_effect=run), self.assertRaises(subprocess.CalledProcessError):
                    runpy.run_path(script)
                execute.assert_not_called()


if __name__ == "__main__":
    unittest.main()
