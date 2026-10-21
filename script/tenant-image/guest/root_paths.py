"""Root-owned executable paths: an ownership invariant both VM image recipes enforce.

Root runs programs from the directories on its PATH: the runner, the tenant
agent, the Boat endpoint, and /usr/local/bin/motor-runner as the job PID
namespace init. If one of those files or directories, or a directory above
them, belonged to another account or could be written by group or other, that
account could replace a program root runs. The usual way this goes wrong is a
recipe extracting, as root, an archive that records its creator's owner; Node's
release tarball stores uid 1001, and extracting it into /usr/local used to
hand /usr/local/bin to that uid.

The module uses only the standard library and imports nothing from beside
itself: the tenant recipe loads it inside the image chroot, the tenant
qualification stages it into a booted guest, the Boat recipe loads it from the
source checkout, and the Boat probe executes its source on a live sandbox.
"""

import os
from pathlib import Path
import stat

# The tenant units (motor-tenant-agent.service, motor-qualification.service)
# run with PATH=/usr/local/lib/motor/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin,
# and the toolchains behind the Cargo proxies live under /usr/local as well.
TENANT_ROOTS = ("/usr/local", "/usr/bin", "/bin", "/usr/sbin", "/sbin")
# motor-boat.service runs as root with
# PATH=/home/ubuntu/.bun/bin:/srv/build/cargo-home/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
# and RUSTUP_HOME=/home/ubuntu/.rustup, whose toolchains the Cargo proxies run.
BOAT_ROOTS = (*TENANT_ROOTS, "/srv/build/cargo-home/bin", "/home/ubuntu/.bun/bin", "/home/ubuntu/.rustup")
# How many offending paths a report names; the count covers all of them.
REPORTED = 20


def raise_error(error):
    raise error


def tree_entries(root):
    """Every path under root, including root, without following symlinks.
    An unreadable directory raises rather than being skipped, so a check
    built on this walk cannot pass without having seen the whole tree."""
    yield root
    for directory, directories, files in os.walk(root, onerror=raise_error):
        for name in directories + files:
            yield Path(directory, name)


def unsafe(path, metadata, owner=0):
    """Why one path breaks the invariant, or None. A symlink's own mode bits
    are meaningless on Linux, so only its owner is checked."""
    if metadata.st_uid != owner:
        return f"{path}: uid={metadata.st_uid} gid={metadata.st_gid}"
    if not stat.S_ISLNK(metadata.st_mode) and metadata.st_mode & 0o022:
        return f"{path}: mode={stat.S_IMODE(metadata.st_mode):04o} (group or other may write)"
    return None


def check(roots, image=Path("/"), owner=0, limit=REPORTED):
    """Check every entry under each root, each root itself and every directory
    from `image` down to it. `roots` are absolute paths inside the image at
    `image` ("/" for a live system or a chroot; a fixture directory in tests).
    A root that is a symlink is checked as a link; its target is walked when it
    exists inside the image and reported as a violation when it resolves
    outside. Missing roots and dangling links are listed as missing, not as
    violations; their existing ancestors are still checked."""
    image = Path(os.path.realpath(image))
    seen, walked, offenders, missing = set(), [], [], []
    entries = 0

    def visit(path):
        """Check one path once; False when it does not exist."""
        nonlocal entries
        if path in seen:
            return True
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            return False
        seen.add(path)
        entries += 1
        reason = unsafe(path, metadata, owner)
        if reason:
            offenders.append(reason)
        return True

    def ancestors(path):
        # The image root, then each directory between it and `path`, up to
        # the first one that does not exist.
        current = image
        visit(current)
        for part in path.relative_to(image).parts[:-1]:
            current = current / part
            if not visit(current):
                return

    for name in roots:
        lexical = image / name.lstrip("/")
        ancestors(lexical)
        if not visit(lexical):
            missing.append(name)
            continue
        real = Path(os.path.realpath(lexical))
        if real != image and image not in real.parents:
            offenders.append(f"{lexical}: resolves outside the checked image to {real}")
            continue
        if not real.exists():
            missing.append(name)
            continue
        ancestors(real)
        if any(real == done or done in real.parents for done in walked):
            continue
        walked.append(real)
        for path in tree_entries(real):
            visit(path)
    return dict(roots=list(roots), entries=entries, offender_count=len(offenders),
                offenders=offenders[:limit], missing=missing)


def require(roots, image=Path("/"), owner=0):
    """check(), raising with the first offending paths when any exist."""
    report = check(roots, image, owner)
    if report["offender_count"]:
        raise ValueError(
            f"{report['offender_count']} path(s) root executes from are not owned by uid {owner} "
            f"without group/other write; first: {'; '.join(report['offenders'])}")
    return report
