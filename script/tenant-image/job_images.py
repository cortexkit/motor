"""Split the completed tenant tree only after its original ext4 is packed."""

import hashlib
import errno
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import struct
import subprocess

HERE = Path(__file__).resolve().parent
REQUIRED = (
    "lib/motor/rustup", "lib/motor/cargo/bin", "lib/motor/cargo/registry",
    "lib/motor/cargo/git", "lib/motor/bun/bin", "lib/motor/bun/install/cache",
    "bin/node", "bin/npm", "bin/pnpm", "lib/node_modules",
)
CARGO_SOURCES = ("registry", "git")
ACL_ACCESS = "system.posix_acl_access"
ACL_DEFAULT = "system.posix_acl_default"


def job_account_uid(root):
    users = [line.split(":") for line in (root / "etc/passwd").read_text().splitlines()
             if line.split(":", 1)[0] == "motor-job"]
    if (len(users) != 1 or len(users[0]) != 7 or not users[0][2].isascii()
            or not users[0][2].isdigit() or not 0 < int(users[0][2]) < 0xffffffff):
        raise ValueError("job root must contain exactly one non-root motor-job uid")
    return int(users[0][2])


def cargo_source(path, tools):
    return any(path.is_relative_to(tools / "usr/local/lib/motor/cargo" / name)
               for name in CARGO_SOURCES)


def cargo_acl(mode, uid, default=False):
    # Linux POSIX ACL xattrs have a version word followed by tag/permissions/id
    # records. Only the job account is named; it never receives write access.
    permissions = 0o5 if default or stat.S_ISDIR(mode) or mode & 0o111 else 0o4
    owner = 0o7 if default else (mode >> 6) & 0o7
    entries = [(0x01, owner, 0xffffffff), (0x02, permissions, uid),
               (0x04, permissions, 0xffffffff), (0x10, permissions, 0xffffffff),
               (0x20, permissions, 0xffffffff)]
    return struct.pack("<I", 2) + b"".join(struct.pack("<HHI", *entry) for entry in entries)


def read_acl(path, name):
    try:
        return os.getxattr(path, name, follow_symlinks=False)
    except OSError as error:
        if error.errno in (errno.ENODATA, errno.EOPNOTSUPP):
            return None
        raise


def verify_cargo_acl(path, uid):
    metadata = path.lstat()
    if metadata.st_uid == uid:
        raise ValueError(f"Cargo source is owned by motor-job: {path}")
    expected_default = cargo_acl(metadata.st_mode, uid, default=True) if stat.S_ISDIR(metadata.st_mode) else None
    if (read_acl(path, ACL_ACCESS) != cargo_acl(metadata.st_mode, uid)
            or read_acl(path, ACL_DEFAULT) != expected_default):
        raise ValueError(f"Cargo source requires exact motor-job read/inheritance ACLs: {path}")


def provision_cargo_acls(tools, uid, walk):
    for name in CARGO_SOURCES:
        for path in walk(tools / "usr/local/lib/motor/cargo" / name):
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                continue
            os.setxattr(path, ACL_ACCESS, cargo_acl(mode, uid), follow_symlinks=False)
            if stat.S_ISDIR(mode):
                os.setxattr(path, ACL_DEFAULT, cargo_acl(mode, uid, default=True), follow_symlinks=False)


def installer():
    spec = importlib.util.spec_from_file_location("tenant_install", HERE / "install-guest.py")
    if spec is None or spec.loader is None:
        raise ImportError("cannot load the shared tenant installer")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def verify_toolchain(tools, job_uid, owner=0, group=0):
    shared = installer()
    def access(path):
        if cargo_source(path, tools):
            verify_cargo_acl(path, job_uid)
            return False
        return shared.has_access_acl(path)
    shared.verify_tool_trees((tools,), acl=access, owner=owner, group=group)
    for path in shared.tool_tree_entries(tools):
        mode = path.lstat().st_mode
        if stat.S_IFMT(mode) not in (stat.S_IFREG, stat.S_IFDIR, stat.S_IFLNK):
            raise ValueError(f"unsupported toolchain file type: {path}")
        if mode & 0o7000:
            raise ValueError(f"special toolchain permission bits: {path}")
        if (not stat.S_ISLNK(mode) and not cargo_source(path, tools)
                and ACL_DEFAULT in os.listxattr(path, follow_symlinks=False)):
            raise ValueError(f"default toolchain ACL: {path}")


def split_tree(root, tools, own=os.lchown, owner=0, group=0):
    uid = job_account_uid(root)
    # Move, rather than copy, preserves every symlink and executable bit. The
    # tenant ext4 has already been packed, so none of its bytes change here.
    local = tools / "usr/local"
    local.parent.mkdir(parents=True, mode=0o755)
    (root / "usr/local").rename(local)
    (root / "usr/local").mkdir(mode=0o755)
    cargo = local / "lib/motor/cargo"
    for name in CARGO_SOURCES:
        (cargo / name).mkdir(mode=0o755)
    (local / "lib/motor/bun/install/cache").mkdir(parents=True, mode=0o755, exist_ok=True)
    for name in REQUIRED:
        if not (local / name).exists():
            raise ValueError(f"job toolchain lacks /usr/local/{name}")
    shared = installer()
    # All of /usr/local is shared, including Node modules and symlink targets,
    # not just the Rust/Bun roots the tenant installer normally publishes.
    shared.publish_tool_trees((tools,), own=own)
    for path in shared.tool_tree_entries(tools):
        if not stat.S_ISLNK(path.lstat().st_mode):
            for attribute in ("system.posix_acl_access", "system.posix_acl_default"):
                if attribute in os.listxattr(path, follow_symlinks=False):
                    os.removexattr(path, attribute, follow_symlinks=False)
    # Cargo overlays copy ACLs up from their immutable lowers. Their directories
    # need read-only inheritance grants before the runner can safely start.
    provision_cargo_acls(tools, uid, shared.tool_tree_entries)
    verify_toolchain(tools, uid, owner=owner, group=group)
    init = root / "sbin/motor-init"
    text = init.read_text()
    original = 'exec /sbin/init "$@"'
    if text.count(original) != 1:
        raise ValueError("unrecognized tenant init handoff")
    init.write_text(text.replace(original, 'exec /sbin/motor-job-toolchain-init "$@"'))
    shutil.copyfile(HERE / "guest/motor-job-toolchain-init", root / "sbin/motor-job-toolchain-init")
    (root / "sbin/motor-job-toolchain-init").chmod(0o755)


def build(stage, size_gib, run=subprocess.run):
    tools = stage / "job-tools"
    split_tree(stage / "root", tools)
    output = stage / "job"
    output.mkdir(mode=0o755)
    # The existing pinned size accommodates the unsplit tenant tree, so it also
    # bounds each half. GiB alignment satisfies Firecracker's 2 MiB pmem rule.
    for name, tree, label in (("rootfs.ext4", stage / "root", "motor-job-root"),
                              ("toolchain.ext4", tools, "motor-toolchain")):
        run(["truncate", "-s", f"{size_gib}G", str(output / name)], check=True)
        run(["mkfs.ext4", "-F", "-L", label, "-d", str(tree), str(output / name)], check=True)


def artifact(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(block)
    length = path.stat().st_size
    if not length:
        raise ValueError(f"empty job image artifact: {path}")
    return dict(bytes=length, sha256=hasher.hexdigest())


def runner_hello(runner):
    result = subprocess.run([str(runner), "print-hello", "--backend", "host", "--tenant-vm",
                             "--return-chunked-max-bytes", "134217728"],
                            check=True, capture_output=True, text=True)
    hello = json.loads(result.stdout)
    if not isinstance(hello, dict) or hello.get("type") != "hello" or hello.get("protocol_version") != 1:
        raise ValueError("image runner did not emit a valid pinned Hello")
    return hello


def publish(stage, target):
    paths = {"kernel": stage / "kernel/vmlinux", "rootfs": stage / "job/rootfs.ext4",
             "toolchain": stage / "job/toolchain.ext4"}
    manifest = {key: artifact(path) for key, path in paths.items()}
    # Query the exact binary placed in the image, without opening runner state.
    # Guest identity and resolver policy remain runtime-owned, not image data.
    manifest.update(hello=runner_hello(stage / "motor-runner"), guest_init={})
    for key, source in paths.items():
        name = "kernel" if key == "kernel" else f"{key}.ext4"
        # The kernel is also a tenant output, so leave its source in place.
        if key == "kernel":
            shutil.copyfile(source, target / name)
        else:
            source.rename(target / name)
        (target / name).chmod(0o444)
        with (target / name).open("rb") as file:
            os.fsync(file.fileno())
    return manifest


def complete(target, manifest):
    (target / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    (target / "manifest.json").chmod(0o444)
    with (target / "manifest.json").open("rb") as file:
        os.fsync(file.fileno())
    fd = os.open(target, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


if __name__ == "__main__":
    import sys
    build(Path(sys.argv[1]), int(sys.argv[2]))
