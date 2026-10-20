#!/usr/bin/env python3
"""Portable preparation tests; no chroot, root privileges or image build."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("guest_test_build", Path(__file__).with_name("build-guest-test.py"))
assert spec is not None and spec.loader is not None
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)


class GuestTestBuildTests(unittest.TestCase):
    def test_native_build_uses_guest_tools_and_never_musl(self):
        command = b.cargo_command(Path("/scratch/root"), 4)
        self.assertEqual(command[:4], ["chroot", "/scratch/root", "/usr/bin/env", "-i"])
        self.assertIn("RUSTC_WRAPPER=", command)
        self.assertIn("CARGO_BUILD_RUSTC_WRAPPER=", command)
        self.assertIn("--offline", command)
        self.assertIn("--locked", command)
        self.assertIn("--no-run", command)
        self.assertIn("x86_64-unknown-linux-gnu", command)
        self.assertNotIn("x86_64-unknown-linux-musl", command)
        self.assertEqual(command[command.index("--manifest-path") + 1], "/qualification/source/Cargo.toml")
        self.assertIn("CARGO_HOME=/usr/local/lib/motor/cargo", command)
        self.assertIn("RUSTUP_HOME=/usr/local/lib/motor/rustup", command)

    def test_artifact_must_be_one_exact_library_test_under_guest_target(self):
        artifact = dict(reason="compiler-artifact", target=dict(name="motor_runner"),
                        executable="/qualification/target/x86_64-unknown-linux-gnu/debug/deps/motor_runner-123")
        parsed = b.artifact_path(json.dumps(artifact), Path("/output"))
        self.assertEqual(parsed, Path("/output/target/x86_64-unknown-linux-gnu/debug/deps/motor_runner-123"))
        for bad in ("", json.dumps(artifact) + "\n" + json.dumps(artifact),
                    json.dumps(dict(artifact, executable="/elsewhere/tests")),
                    json.dumps(dict(artifact, executable="/qualification/target/../outside"))):
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                b.artifact_path(bad, Path("/output"))

    def test_bind_is_readonly_and_cleanup_reverses_mount_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            execute = mock.Mock(return_value="")
            builder = b.Builder(root / "image", root / "source", root / "cache", root / "output", execute=execute)
            builder.output.mkdir()
            builder.bind(root / "source", builder.root / "source", readonly=True)
            builder.mount("-t", "proc", "proc", builder.root / "proc")
            self.assertEqual(execute.call_args_list[0], mock.call("mount", "--bind", root / "source", builder.root / "source"))
            self.assertEqual(execute.call_args_list[1], mock.call("mount", "-o", "remount,bind,ro", builder.root / "source"))
            with mock.patch("builtins.print"):
                builder.cleanup()
            self.assertEqual(execute.call_args_list[-2:], [mock.call("umount", builder.root / "proc"), mock.call("umount", builder.root / "source")])
            self.assertEqual(builder.mounts, [])

    def test_failed_unmount_retains_overlay_and_attempts_other_unmounts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            builder = b.Builder(root / "image", root / "source", root / "cache", root / "output", execute=mock.Mock(side_effect=[RuntimeError("busy"), ""]))
            builder.root.mkdir(parents=True)
            (builder.root / "evidence").write_text("retain")
            builder.mounts = [builder.root, builder.root / "proc"]
            with self.assertRaisesRegex(RuntimeError, "retain output"):
                builder.cleanup()
            self.assertTrue((builder.root / "evidence").is_file())
            self.assertEqual(builder.execute.call_count, 2)

    def test_space_guard_happens_before_creating_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            builder = b.Builder(root / "image", root / "source", root / "cache", root / "output", execute=mock.Mock())
            with mock.patch.object(b.shutil, "disk_usage", return_value=type("Usage", (), dict(free=b.REQUIRED_FREE - 1))):
                with self.assertRaisesRegex(RuntimeError, "232 GiB"):
                    builder.build()
            self.assertFalse(builder.output.exists())
            builder.execute.assert_not_called()

    def test_build_mounts_image_readonly_and_checks_loader_before_cleanup(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            image, source, cache, output = [root / name for name in ("image", "source", "cache", "output")]
            image.mkdir()
            source.mkdir()
            (cache / "registry").mkdir(parents=True)
            (image / "rootfs").write_bytes(b"immutable image")
            (image / "manifest.json").write_text(json.dumps(dict(rootfs_sha256=b.sha256(image / "rootfs"))))
            calls = []
            def execute(*args, stdout=None):
                calls.append(args)
                if args[:4] == ("mount", "-t", "overlay", "overlay"):
                    (output / "root/proc").mkdir()
                if "--message-format=json" in args:
                    binary = output / "target/x86_64-unknown-linux-gnu/debug/deps/motor_runner-123"
                    binary.parent.mkdir(parents=True)
                    binary.write_bytes(b"compiled libtest")
                    stdout.write(json.dumps(dict(reason="compiler-artifact", target=dict(name="motor_runner"),
                                                 executable="/qualification/target/x86_64-unknown-linux-gnu/debug/deps/motor_runner-123")) + "\n")
                if "--list" in args:
                    return b.TEST + ": test\n"
                return "fixture tool version or commit\n"
            builder = b.Builder(image, source, cache, output, execute=execute)
            with mock.patch.object(b.shutil, "disk_usage", return_value=type("Usage", (), dict(free=b.REQUIRED_FREE))), mock.patch("builtins.print"):
                self.assertEqual(builder.build(), output / "runner-tests")
            self.assertIn(("mount", "-o", "loop,ro", image / "rootfs", output / "lower"), calls)
            loader = next(i for i, args in enumerate(calls) if "--list" in args)
            unmount = next(i for i, args in enumerate(calls) if args[0] == "umount")
            self.assertLess(loader, unmount)
            self.assertEqual((output / "runner-tests").read_bytes(), b"compiled libtest")
            self.assertEqual((image / "rootfs").read_bytes(), b"immutable image")
            self.assertFalse((output / "root").exists())
            self.assertTrue((output / "provenance.json").is_file())

    def test_output_must_not_overlap_source_or_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for output in (root / "source/build", root / "image/build", root):
                builder = b.Builder(root / "image", root / "source", root / "cache", output)
                with self.subTest(output=output), self.assertRaisesRegex(ValueError, "separate"):
                    builder.build()


if __name__ == "__main__":
    unittest.main(verbosity=2)
