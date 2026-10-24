#!/usr/bin/env python3
"""Install a pinned npm release into the npm global layout, root-owned.

`npm install --global --prefix /usr/local` as root cannot promise the two
properties these images need. npm creates its directories with the caller's
umask, so the tree can be group- or other-writable; and it marks an installed
command's target executable with chmod, an operation no archive extraction
expresses. Everything root runs from /usr/local must be root's with no group or
other write (script/tenant-image/guest/root_paths.py), and pnpm's commands are
exactly its chmod'ed targets: `/usr/local/bin/pnpm` is a link to the launcher
`bin/pnpm.mjs`, and node runs that launcher only because the mode bit lets it.

So the pinned registry tarball is unpacked here instead, by rule: as root's
files under umask 022, into `<prefix>/lib/node_modules/<package>`, with the
package's `bin` map written into `<prefix>/bin` as relative links exactly as npm
writes them, and each linked target made executable. The tree is then the one
`npm install --global` would have produced, with the ownership and modes the
recipe decides.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import tarfile

# Where npm keeps global packages beneath a prefix, and where it writes the
# command links.
PACKAGES = PurePosixPath("lib/node_modules")
COMMANDS = PurePosixPath("bin")
# A published npm tarball holds one package directory and nothing above it.
TOP_LEVEL = "package"


def sha512(path):
    hashed = hashlib.sha512()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hashed.update(chunk)
    return hashed.hexdigest()


def unsafe(name):
    """Why an archive member name must not be extracted, or None."""
    path = PurePosixPath(name)
    if path.is_absolute():
        return "is absolute"
    if not path.parts or path.parts[0] != TOP_LEVEL or ".." in path.parts:
        return "leaves the package directory"
    return None


def extract(archive, destination):
    """Unpack a package tarball into `destination` as root's files under umask
    022, refusing anything but regular files and directories inside it."""
    previous = os.umask(0o022)
    try:
        with tarfile.open(archive, "r:gz") as bundle:
            members = []
            for member in bundle.getmembers():
                reason = unsafe(member.name)
                if reason:
                    raise ValueError(f"release member {reason}: {member.name}")
                if not (member.isfile() or member.isdir()):
                    raise ValueError("release member is not a file or directory: " + member.name)
                members.append(member)
            # The tar filter drops the archive's owner fields, so running as
            # root yields root's files whatever uid the release was packed
            # with; Python before 3.14 would otherwise restore it by chown.
            bundle.extractall(destination, members=members, filter="tar")
    finally:
        os.umask(previous)


def bin_entries(package):
    """The package's `bin` map: command name to its path inside the package.
    A single-string `bin` names its one command after the package."""
    entries = json.loads((package / "package.json").read_text()).get("bin")
    if isinstance(entries, str):
        entries = {package.name: entries}
    if not isinstance(entries, dict) or not entries:
        raise ValueError("release package declares no commands")
    root = package.resolve()
    for name, relative in entries.items():
        if (not isinstance(name, str) or not name or "/" in name or name.startswith(".")
                or not isinstance(relative, str)
                or not (package / relative).resolve().is_relative_to(root)):
            raise ValueError("release package has an unusable command entry: " + str(name))
    return entries


def install(archive, prefix, name):
    """Install `archive` as `<prefix>/lib/node_modules/<name>`, write the
    package's commands as links in `<prefix>/bin`, and make each linked target
    executable as npm does, so `<prefix>/bin/<name>` runs this package and not
    another. Returns the package directory."""
    prefix = Path(prefix)
    packages = prefix / PACKAGES
    root = packages / name
    links = prefix / COMMANDS
    staging = packages / TOP_LEVEL
    # <prefix>/bin is a shared directory (node, npm and other tools already
    # live in /usr/local/bin), so only this package's own paths are refused.
    for path in (root, staging):
        if path.exists() or path.is_symlink():
            raise FileExistsError("refusing to replace an existing install: " + str(path))
    # mkdir honours the caller's umask; every directory this module creates is
    # set explicitly, because root runs programs from the tree and nothing in it
    # may be group- or other-writable. Directories that already exist keep
    # their owner and mode.
    if not packages.is_dir():
        packages.mkdir(parents=True)
        os.chmod(packages, 0o755)
    extract(archive, packages)
    if not staging.is_dir():
        raise ValueError("release archive has no package directory")
    for directory in (staging, *[path for path in staging.rglob("*") if path.is_dir()]):
        os.chmod(directory, 0o755)
    try:
        commands = bin_entries(staging)
        if name not in commands:
            raise ValueError(f"release package declares no {name} command")
        taken = [links / command for command in commands
                 if (links / command).exists() or (links / command).is_symlink()]
        if taken:
            raise FileExistsError("refusing to replace an existing command: " + str(taken[0]))
    except BaseException:
        shutil.rmtree(staging)
        raise
    staging.rename(root)
    if not links.is_dir():
        links.mkdir(parents=True)
        os.chmod(links, 0o755)
    for command, relative in commands.items():
        target = root / relative
        target.chmod(target.stat().st_mode | 0o111)
        (links / command).symlink_to(os.path.relpath(target, links))
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--sha512", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    if sha512(arguments.archive) != arguments.sha512:
        raise ValueError("release SHA-512 mismatch: " + arguments.url)
    root = install(arguments.archive, arguments.prefix, arguments.name)
    report = {"package": arguments.name, "root": str(root), "sha512": arguments.sha512,
              "commands": sorted(path.name for path in (arguments.prefix / COMMANDS).iterdir())}
    if arguments.output:
        arguments.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
