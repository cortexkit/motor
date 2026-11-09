"""Split the completed tenant tree only after its original ext4 is packed."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess

HERE = Path(__file__).resolve().parent
REQUIRED = (
    "lib/motor/rustup", "lib/motor/cargo/bin", "lib/motor/cargo/registry",
    "lib/motor/cargo/git", "lib/motor/bun/bin", "lib/motor/bun/install/cache",
    "bin/node", "bin/npm", "bin/pnpm", "lib/node_modules",
)


def installer():
    spec = importlib.util.spec_from_file_location("tenant_install", HERE / "install-guest.py")
    if spec is None or spec.loader is None:
        raise ImportError("cannot load the shared tenant installer")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def verify_toolchain(tools, owner=0, group=0):
    shared = installer()
    shared.verify_tool_trees((tools,), owner=owner, group=group)
    for path in shared.tool_tree_entries(tools):
        mode = path.lstat().st_mode
        if stat.S_IFMT(mode) not in (stat.S_IFREG, stat.S_IFDIR, stat.S_IFLNK):
            raise ValueError(f"unsupported toolchain file type: {path}")
        if mode & 0o7000:
            raise ValueError(f"special toolchain permission bits: {path}")
        if not stat.S_ISLNK(mode) and "system.posix_acl_default" in os.listxattr(path, follow_symlinks=False):
            raise ValueError(f"default toolchain ACL: {path}")


def split_tree(root, tools, own=os.lchown, owner=0, group=0):
    # Move, rather than copy, preserves every symlink and executable bit. The
    # tenant ext4 has already been packed, so none of its bytes change here.
    local = tools / "usr/local"
    local.parent.mkdir(parents=True, mode=0o755)
    (root / "usr/local").rename(local)
    (root / "usr/local").mkdir(mode=0o755)
    cargo = local / "lib/motor/cargo"
    for name in ("registry", "git"):
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
    verify_toolchain(tools, owner=owner, group=group)
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


def publish(stage, target):
    paths = {"kernel": stage / "kernel/vmlinux", "rootfs": stage / "job/rootfs.ext4",
             "toolchain": stage / "job/toolchain.ext4"}
    manifest = {key: artifact(path) for key, path in paths.items()}
    # These fields describe image configuration, not a per-VM policy. The
    # helper supplies runner arguments at each start; neither object adds any.
    manifest.update(hello={}, guest_init={})
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
