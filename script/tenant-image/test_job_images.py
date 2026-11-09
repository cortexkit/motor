#!/usr/bin/env python3
"""Portable checks for the job-image split; no mounts, image builds or VM boots."""

import hashlib
import errno
import json
import os
from pathlib import Path
import runpy
import socket
import stat
import subprocess
import tempfile
import unittest
from unittest import mock

import build as image
import job_images


class JobImageTests(unittest.TestCase):
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
        (self.root / "etc/passwd").write_text("motor-job:x:1001:1001::/:/usr/sbin/nologin\n")
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
        shared = job_images.installer()
        shared.verify_tool_trees((self.tools,), owner=os.geteuid(), group=os.getegid())
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

    def test_split_removes_access_and_default_acls_without_following_links(self):
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
                        job_images.verify_toolchain(self.tools, owner=os.geteuid(), group=os.getegid())
                    path.chmod(0o755)

    def test_verifier_refuses_fifo_and_socket_fixtures(self):
        self.split()
        fifo = self.tools / "fifo"
        os.mkfifo(fifo, 0o644)
        with self.assertRaisesRegex(ValueError, "unsupported toolchain file type"):
            job_images.verify_toolchain(self.tools, owner=os.geteuid(), group=os.getegid())
        fifo.unlink()
        sock = self.tools / "socket"
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(sock))
            sock.chmod(0o644)
            with self.assertRaisesRegex(ValueError, "unsupported toolchain file type"):
                job_images.verify_toolchain(self.tools, owner=os.geteuid(), group=os.getegid())

    def test_verifier_refuses_access_and_default_acl_fixtures(self):
        self.split()
        leaf = self.tools / "usr/local/bin/node"
        directory = leaf.parent
        def access(path, name, **_kwargs):
            if path == leaf and name == "system.posix_acl_access":
                return b"fixture access ACL"
            raise OSError(errno.ENODATA, "no ACL")
        with mock.patch.object(os, "getxattr", side_effect=access), \
             self.assertRaisesRegex(ValueError, "plainly world-readable"):
            job_images.verify_toolchain(self.tools, owner=os.geteuid(), group=os.getegid())
        with mock.patch.object(os, "listxattr", side_effect=lambda path, **_kwargs:
                               ["system.posix_acl_default"] if path == directory else []), \
             self.assertRaisesRegex(ValueError, "default toolchain ACL"):
            job_images.verify_toolchain(self.tools, owner=os.geteuid(), group=os.getegid())

    def test_job_manifest_matches_helper_schema_and_hashes_real_files(self):
        (self.stage / "kernel").mkdir()
        (self.stage / "kernel/vmlinux").write_bytes(b"pinned tenant kernel")
        (self.stage / "job").mkdir()
        (self.stage / "job/rootfs.ext4").write_bytes(b"job root")
        (self.stage / "job/toolchain.ext4").write_bytes(b"toolchain")
        target = self.stage / "staging/generation"
        target.mkdir(parents=True)
        manifest = job_images.publish(self.stage, target)
        self.assertFalse((target / "manifest.json").exists())
        job_images.complete(target, manifest)
        self.assertEqual(set(p.name for p in target.iterdir()),
                         {"kernel", "rootfs.ext4", "toolchain.ext4", "manifest.json"})
        self.assertEqual(set(manifest), {"kernel", "rootfs", "toolchain", "hello", "guest_init"})
        self.assertEqual(manifest["hello"], {})
        self.assertEqual(manifest["guest_init"], {})
        for key, name in (("kernel", "kernel"), ("rootfs", "rootfs.ext4"), ("toolchain", "toolchain.ext4")):
            data = (target / name).read_bytes()
            self.assertEqual(manifest[key], dict(bytes=len(data), sha256=hashlib.sha256(data).hexdigest()))
            self.assertEqual(stat.S_IMODE((target / name).stat().st_mode), 0o444)
        self.assertEqual(json.loads((target / "manifest.json").read_text()), manifest)
        self.assertEqual((self.stage / "kernel/vmlinux").read_bytes(), b"pinned tenant kernel")

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
