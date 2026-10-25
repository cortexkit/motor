#!/usr/bin/env python3
"""install-npm-tool.py: the pnpm release installed as root's npm global tree.

The recipe installs pnpm with this script instead of `npm install --global`,
because npm cannot promise two things the images require of /usr/local: that
every file and directory is root's with no group or other write, and that the
launcher its command links point at is executable, which for node means the
mode bit no archive extraction restores. These tests plant both failures: an
archive whose publisher owner and modes would leak through a plain extraction,
and a package whose launcher is not executable.
"""
import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("install_npm_tool", HERE / "install-npm-tool.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

# The publisher's Node release account records this owner on every member, as
# pnpm's own tarball does (uid 501 on the registry, 1001 in Node's archives).
PUBLISHER = 501
BIN = {"pnpm": "bin/pnpm.mjs", "pnpx": "bin/pnpx.mjs", "pn": "bin/pnpm.mjs", "pnx": "bin/pnpx.mjs"}


def package(members, manifest=None):
    """A published tarball's bytes: every member owned by PUBLISHER with mode
    0644, as a registry release is."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name, (text, mode) in members.items():
            data = text.encode()
            member = tarfile.TarInfo(name)
            member.size, member.mode, member.uid, member.gid = len(data), mode, PUBLISHER, PUBLISHER
            archive.addfile(member, io.BytesIO(data))
        if manifest is not None:
            data = json.dumps(manifest).encode()
            member = tarfile.TarInfo("package/package.json")
            member.size, member.mode, member.uid, member.gid = len(data), 0o644, PUBLISHER, PUBLISHER
            archive.addfile(member, io.BytesIO(data))
    return buffer.getvalue()


def release(manifest=None, launcher_mode=0o644):
    members = {
        "package/bin/pnpm.mjs": ("#!/usr/bin/env node\n", launcher_mode),
        "package/bin/pnpx.mjs": ("#!/usr/bin/env node\n", launcher_mode),
        "package/dist/pnpm.cjs": ("#!/usr/bin/env node\n", 0o644),
    }
    return package(members, manifest or {"name": "pnpm", "version": "11.1.2", "bin": BIN})


class InstallTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.work = Path(os.path.realpath(temp.name))
        self.prefix = self.work / "prefix"

    def write(self, payload, name="pnpm.tgz"):
        path = self.work / name
        path.write_bytes(payload)
        return path

    def modes(self, root):
        return {path.relative_to(self.work).as_posix(): path.lstat().st_mode & 0o7777
                for path in sorted([root, *root.rglob("*")])}

    def test_release_lands_root_owned_with_no_group_or_other_write(self):
        # The recipe calls this under whatever umask it was started with; a
        # permissive one must not reach the installed tree. Directories above
        # the package tree are the caller's (here the test's own), so only the
        # installer's own output is checked.
        previous = os.umask(0o002)
        try:
            installer.install(self.write(release()), self.prefix, "pnpm")
        finally:
            os.umask(previous)
        root = self.prefix / "lib/node_modules/pnpm"
        found = [self.prefix / "bin", self.prefix / "lib/node_modules", root]
        for directory, directories, files in os.walk(root):
            found += [Path(directory, name) for name in directories + files]
        for path in found:
            metadata = path.lstat()
            with self.subTest(path=path.relative_to(self.work).as_posix()):
                self.assertEqual(metadata.st_uid, os.geteuid())
                if not path.is_symlink():
                    self.assertEqual(metadata.st_mode & 0o022, 0,
                                     f"{path} is writable by group or other")

    def test_commands_are_npms_links_and_their_targets_are_executable(self):
        # `npm install --global` writes bin/pnpm -> ../lib/node_modules/pnpm/bin/pnpm.mjs
        # and leaves the launcher non-executable, so node refuses to run it.
        root = installer.install(self.write(release()), self.prefix, "pnpm")
        links = self.prefix / "bin"
        self.assertEqual(sorted(path.name for path in links.iterdir()), sorted(BIN))
        for name, relative in BIN.items():
            link = links / name
            with self.subTest(command=name):
                self.assertTrue(link.is_symlink())
                self.assertEqual(os.readlink(link), "../lib/node_modules/pnpm/" + relative)
                self.assertEqual(Path(os.path.realpath(link)), root / relative)
                self.assertTrue(os.access(root / relative, os.X_OK),
                                f"{relative} must be executable for node to start it")

    def test_a_member_outside_the_package_directory_is_refused(self):
        for name in ("package/../escape", "package/x/../../escape", "/etc/motd", "other/file"):
            with self.subTest(member=name):
                payload = package({name: ("x", 0o644), "package/package.json": (
                    json.dumps({"name": "pnpm", "bin": {"pnpm": "bin/pnpm.mjs"}}), 0o644)})
                destination = self.work / ("members-" + name.replace("/", "_"))
                destination.mkdir()
                with self.assertRaisesRegex(ValueError, "release member"):
                    installer.extract(self.write(payload), destination)
                self.assertEqual([path.name for path in destination.iterdir()], [])

    def test_a_link_member_is_refused(self):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            link = tarfile.TarInfo("package/bin/pnpm.mjs")
            link.type, link.linkname = tarfile.SYMTYPE, "/usr/bin/env"
            archive.addfile(link)
        with self.assertRaisesRegex(ValueError, "not a file or directory"):
            installer.install(self.write(buffer.getvalue()), self.prefix, "pnpm")

    def test_a_package_without_commands_is_refused(self):
        payload = package({"package/bin/pnpm.mjs": ("x", 0o644)}, {"name": "pnpm", "version": "1"})
        with self.assertRaisesRegex(ValueError, "declares no commands"):
            installer.install(self.write(payload), self.prefix, "pnpm")

    def test_a_package_without_the_command_it_is_installed_as_is_refused(self):
        payload = release({"name": "pnpm", "bin": {"pnpx": "bin/pnpx.mjs"}})
        with self.assertRaisesRegex(ValueError, "declares no pnpm command"):
            installer.install(self.write(payload), self.prefix, "pnpm")

    def test_a_command_outside_the_package_is_refused(self):
        payload = package({"package/bin/pnpm.mjs": ("x", 0o644)},
                          {"name": "pnpm", "bin": {"pnpm": "../../../../etc/passwd"}})
        with self.assertRaisesRegex(ValueError, "unusable command entry"):
            installer.install(self.write(payload), self.prefix, "pnpm")

    def test_an_existing_install_is_not_replaced(self):
        installer.install(self.write(release()), self.prefix, "pnpm")
        with self.assertRaises(FileExistsError):
            installer.install(self.write(release(), "again.tgz"), self.prefix, "pnpm")
        self.assertEqual(sorted(path.name for path in (self.prefix / "bin").iterdir()), sorted(BIN))

    def test_a_shared_bin_directory_that_already_exists_is_kept(self):
        # In the images node is unpacked into /usr/local first, so
        # /usr/local/bin and /usr/local/lib/node_modules already exist and hold
        # other tools; installing must add links there, not refuse or replace.
        (self.prefix / "bin").mkdir(parents=True)
        (self.prefix / "bin/node").write_text("node")
        (self.prefix / "lib/node_modules/npm").mkdir(parents=True)
        installer.install(self.write(release()), self.prefix, "pnpm")
        self.assertEqual((self.prefix / "bin/node").read_text(), "node")
        self.assertTrue((self.prefix / "lib/node_modules/npm").is_dir())
        self.assertEqual(sorted(path.name for path in (self.prefix / "bin").iterdir()),
                         sorted(["node", *BIN]))

    def test_an_existing_command_of_the_same_name_is_not_replaced(self):
        (self.prefix / "bin").mkdir(parents=True)
        (self.prefix / "bin/pnpm").write_text("other pnpm")
        with self.assertRaisesRegex(FileExistsError, "existing command"):
            installer.install(self.write(release()), self.prefix, "pnpm")
        self.assertEqual((self.prefix / "bin/pnpm").read_text(), "other pnpm")
        # Nothing of the refused release is left behind.
        self.assertFalse((self.prefix / "lib/node_modules/package").exists())
        self.assertFalse((self.prefix / "lib/node_modules/pnpm").exists())

class CommandTests(unittest.TestCase):
    def test_the_cli_refuses_an_archive_that_does_not_match_its_pin(self):
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp)
            archive = work / "pnpm.tgz"
            archive.write_bytes(release())
            result = subprocess.run([sys.executable, str(HERE / "install-npm-tool.py"),
                                     "--prefix", str(work / "prefix"), "--name", "pnpm",
                                     "--archive", str(archive), "--sha512", "0" * 128,
                                     "--url", "https://registry.npmjs.org/pnpm/-/pnpm-11.1.2.tgz"],
                                    text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("SHA-512 mismatch", result.stderr)
            self.assertFalse((work / "prefix").exists())
            result = subprocess.run([sys.executable, str(HERE / "install-npm-tool.py"),
                                     "--prefix", str(work / "prefix"), "--name", "pnpm",
                                     "--archive", str(archive), "--sha512", installer.sha512(archive),
                                     "--url", "https://registry.npmjs.org/pnpm/-/pnpm-11.1.2.tgz"],
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["root"], str(work / "prefix/lib/node_modules/pnpm"))


if __name__ == "__main__":
    unittest.main()
