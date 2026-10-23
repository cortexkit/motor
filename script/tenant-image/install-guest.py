#!/usr/bin/python3
"""Runs inside the disposable chroot, using only its staged inputs and public downloads."""

import hashlib
import errno
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import urllib.request

BUILD = Path("/srv/build")
PREFIX = Path("/usr/local/lib/motor")
GUEST_SERVICES = ("motor-tenant-agent.service", "motor-net-rules.service", "motor-image-check.service")
MODULE_LOAD_DIRS = ("etc/modules-load.d", "run/modules-load.d", "usr/local/lib/modules-load.d",
                    "usr/lib/modules-load.d", "lib/modules-load.d")
AGENT_VALIDATED_PATHS = (Path("/tenant"), Path("/Users"), Path("/home"), PREFIX / "cargo/bin")
# Image tools shared by every tenant's jobs. They never change per tenant, so
# they ship root-owned and readable by every account. The tenant runner then
# finds them readable and skips its startup ACL grant, which on the VM's RAM
# overlay would copy the whole toolchain up into memory on every boot.
TOOL_TREES = (PREFIX / "rustup", PREFIX / "cargo")


def load_root_paths():
    # The chroot stages guest/ beside this script, as the source tree does.
    spec = importlib.util.spec_from_file_location("root_paths", Path(__file__).resolve().parent / "guest/root_paths.py")
    if spec is None or spec.loader is None:
        raise ImportError("cannot load the root-owned path check")
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


root_paths = load_root_paths()


def run(*args, capture=False) -> str:
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def refuse_probe(args, expected):
    result = subprocess.run(["motor-runner", *args], text=True, capture_output=True)
    if result.returncode == 0 or expected not in result.stderr:
        raise ValueError(f"runner capability probe failed: {args}: {result.stderr}")


def extract(archive, destination, *options):
    # Release archives record their creator's owner (Node's stores uid 1001)
    # and modes, and tar run as root restores both. Extract as root-owned files
    # instead, with modes masked by a fixed 022 umask, so no other account can
    # write where root later executes from.
    previous = os.umask(0o022)
    try:
        run("tar", "--no-same-owner", "--no-same-permissions", *options, "-xf", str(archive),
            "-C", str(destination))
    finally:
        os.umask(previous)


def download(url, destination, expected):
    if not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise ValueError(f"invalid published SHA-256 for {url}")
    urllib.request.urlretrieve(url, destination)
    actual = hashlib.sha256(destination.read_bytes()).hexdigest()
    if actual != expected:
        raise ValueError(f"download SHA-256 mismatch: {url}")


def published_hash(url, filename):
    text = urllib.request.urlopen(url).read().decode()
    for line in text.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1].lstrip("*") == filename:
            return fields[0]
    raise ValueError(f"no checksum for {filename} at {url}")


def install_tools(pins):
    # Use the same measured releases, target/component sets and installers as
    # Boat; tenant-only pins describe the OS and kernel, never a second toolchain.
    spec = importlib.util.spec_from_file_location("boat_tools", BUILD / "script/boat-image/install.py")
    if spec is None or spec.loader is None:
        raise ImportError("cannot load the shared image tool installer")
    shared = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(shared)
    manifest = json.loads((BUILD / "script/runner/toolchain.json").read_text())
    shared.parity.validate(manifest)
    run("apt-get", "purge", "-y", "binaryen")
    shared.tools(manifest, role="tenant")

    PREFIX.mkdir(parents=True)
    (PREFIX / "cargo").mkdir()
    shutil.copytree(BUILD / "cargo-home/bin", PREFIX / "cargo/bin", symlinks=True)
    shutil.move("/home/ubuntu/.rustup", PREFIX / "rustup")
    # Root runs these files, so they get a fixed root-only-writable mode rather
    # than whatever mode the build checkout's umask left on the source copy.
    def install_file(source, destination):
        shutil.copyfile(source, destination)
        os.chmod(destination, 0o644)

    for name in ("sccache-inputs.sha256", "sccache-artifacts.json"):
        install_file(BUILD / name, PREFIX / name)
    parity_dir = PREFIX / "parity"
    parity_dir.mkdir(mode=0o755)
    for name in ("toolchain.json", "toolchain-parity.py", "cargo-seed.py", "syscall-surface.py"):
        install_file(BUILD / "script/runner" / name, parity_dir / name)
    # Only immutable binaries and toolchains enter the image, not install caches.
    os.environ.update(CARGO_HOME=str(PREFIX / "cargo"), RUSTUP_HOME=str(PREFIX / "rustup"),
                      PATH=f"/home/ubuntu/.bun/bin:{PREFIX}/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")


def install_services(source, destination):
    destination.mkdir(parents=True, exist_ok=True)
    for name in GUEST_SERVICES:
        shutil.copy2(source / name, destination / name)


def clear_module_load_requests(root):
    # Required drivers are built in. Even an empty vendor .conf can start the
    # loader via ConditionDirectoryNotEmpty, despite the missing module store.
    for relative in MODULE_LOAD_DIRS:
        directory = root / relative
        if directory.exists():
            for path in directory.iterdir():
                if path.is_dir() and not path.is_symlink():
                    raise ValueError(f"unexpected directory in module-load configuration: {path}")
                path.unlink()
    legacy = root / "etc/modules"
    legacy.parent.mkdir(parents=True, exist_ok=True)
    legacy.unlink(missing_ok=True)
    legacy.write_text("")
    dropin = root / "etc/systemd/system/systemd-modules-load.service.d/10-motor-console.conf"
    dropin.parent.mkdir(parents=True, exist_ok=True)
    dropin.write_text("[Service]\nStandardOutput=journal+console\nStandardError=journal+console\n")


def verify_no_module_load_requests(root):
    entries = []
    for relative in MODULE_LOAD_DIRS:
        directory = root / relative
        if directory.exists():
            entries.extend(str(path) for path in directory.iterdir())
    legacy = root / "etc/modules"
    if legacy.exists() and legacy.read_text().strip():
        entries.append(str(legacy))
    if entries:
        raise ValueError(f"image must not request loadable kernel modules: {entries}")
    return []


def agent_path_components(root):
    paths = {root}
    for path in AGENT_VALIDATED_PATHS:
        current = root
        for part in path.parts[1:]:
            current = current / part
            paths.add(current)
    return sorted(paths, key=lambda path: (len(path.parts), str(path)))


def normalize_agent_paths(root, own=os.chown):
    for path in agent_path_components(root):
        metadata = path.lstat()
        if not stat.S_ISDIR(metadata.st_mode):
            raise ValueError(f"image agent path is not a real directory: {path}")
        own(path, 0, 0)
        path.chmod(0o755)


def verify_agent_paths(root, owner=0):
    # AGENT_VALIDATED_PATHS drives both normalization and this component walk.
    # Like the guest agent, require real directories, the expected uid, and no
    # group/other write permission on the final path and every ancestor.
    for path in agent_path_components(root):
        metadata = path.lstat()
        if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != owner or metadata.st_mode & 0o022:
            raise ValueError(f"unsafe image agent directory {path}: uid={metadata.st_uid} mode={metadata.st_mode & 0o7777:04o}; expected uid={owner}, real directory, no group/other write")
    return [str(path) for path in AGENT_VALIDATED_PATHS]


tool_tree_entries = root_paths.tree_entries


def publish_tool_trees(roots=TOOL_TREES, own=os.lchown):
    for root in roots:
        for path in tool_tree_entries(root):
            metadata = path.lstat()
            own(path, 0, 0)
            if stat.S_ISLNK(metadata.st_mode):
                continue
            mode = stat.S_IMODE(metadata.st_mode) & ~0o022
            searchable = stat.S_ISDIR(metadata.st_mode) or mode & 0o111
            path.chmod(mode | (0o555 if searchable else 0o444))


def has_access_acl(path):
    try:
        os.getxattr(path, "system.posix_acl_access", follow_symlinks=False)
    except OSError as error:
        if error.errno in (errno.ENODATA, errno.EOPNOTSUPP):
            return False
        raise
    return True


def verify_tool_trees(roots=TOOL_TREES, acl=has_access_acl, owner=0, group=0):
    # Mirrors the runner's startup check: root owner and group, no extended
    # ACL, group and other may read every file and search every directory.
    counts = {}
    for root in roots:
        count = 0
        for path in tool_tree_entries(root):
            metadata = path.lstat()
            if metadata.st_uid != owner or metadata.st_gid != group:
                raise ValueError(f"image tool path is not root-owned: {path}: {metadata.st_uid}:{metadata.st_gid}")
            if stat.S_ISLNK(metadata.st_mode):
                continue
            mode = stat.S_IMODE(metadata.st_mode)
            needed = 0o055 if stat.S_ISDIR(metadata.st_mode) else 0o044
            if mode & needed != needed or mode & 0o022 or acl(path):
                raise ValueError(f"image tool path is not plainly world-readable: {path}: mode {mode:04o}")
            count += 1
        counts[str(root)] = count
    return counts


def configure(pins):
    spec = importlib.util.spec_from_file_location("job_account", BUILD / "script/runner/job-account.py")
    if spec is None or spec.loader is None:
        raise ImportError("cannot load the shared job identity installer")
    account = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(account)
    account.ensure(json.loads((BUILD / "script/runner/toolchain.json").read_text())['job_identity'], run)
    for account in ("motor-cache",):
        run("useradd", "--system", "--user-group", "--home-dir", "/nonexistent",
            "--shell", "/usr/sbin/nologin", account)
    for directory in ("tenant", "Users", "home", "overlay", ".lower", "usr/local/libexec", "etc/nftables.d"):
        Path("/", directory).mkdir(parents=True, exist_ok=True)
    normalize_agent_paths(Path("/"))
    publish_tool_trees()
    shutil.copy2(BUILD / "motor-runner", "/usr/local/bin/motor-runner")
    Path("/usr/local/bin/motor-runner").chmod(0o755)
    shutil.copy2(BUILD / "guest/motor-init", "/sbin/motor-init")
    Path("/sbin/motor-init").chmod(0o755)
    shutil.copy2(BUILD / "guest/hosted_image_check.py", "/usr/local/libexec/hosted_image_check.py")
    Path("/usr/local/libexec/hosted_image_check.py").chmod(0o755)
    install_services(BUILD / "guest", Path("/etc/systemd/system"))
    clear_module_load_requests(Path("/"))
    rules = (BUILD / "script/runner/motor-net.nft").read_text()
    if rules.count("enp6s0") != 3:
        raise ValueError("unexpected runner network interface template")
    Path("/etc/nftables.d/motor-net.nft").write_text(rules.replace("enp6s0", "eth0"))
    Path("/etc/sysctl.d/90-motor.conf").write_text("net.ipv4.ip_forward=1\nkernel.modules_disabled=1\n")
    shutil.copy2(BUILD / "script/runner/sysctl/60-motor-inotify.conf", "/etc/sysctl.d/60-motor-inotify.conf")
    Path("/etc/hostname").write_text("motor-tenant\n")
    Path("/etc/hosts").write_text("127.0.0.1 localhost\n127.0.1.1 motor-tenant\n")
    Path("/etc/resolv.conf").unlink(missing_ok=True)
    Path("/etc/resolv.conf").write_text("")
    Path("/etc/machine-id").write_text("")
    dbus_id = Path("/var/lib/dbus/machine-id")
    dbus_id.unlink(missing_ok=True)
    Path("/etc/fstab").write_text("tmpfs /tmp tmpfs mode=1777,nodev,nosuid 0 0\n")
    Path("/etc/default/locale").write_text("LANG=C.UTF-8\n")
    Path("/etc/profile.d/motor-tools.sh").write_text(
        f"export RUSTUP_HOME={PREFIX}/rustup\nexport PATH={PREFIX}/cargo/bin:$PATH\n")
    Path("/etc/apt/sources.list").write_text(
        f"deb https://snapshot.debian.org/archive/debian/{pins['debian_snapshot']}/ {pins['debian_suite']} main\n")
    Path("/etc/apt/apt.conf.d/99snapshot").write_text('Acquire::Check-Valid-Until "false";\n')
    run("systemctl", "enable", "motor-tenant-agent.service")
    run("systemctl", "mask", "systemd-networkd.service", "systemd-resolved.service",
        "systemd-remount-fs.service",
        "apt-daily.timer", "apt-daily-upgrade.timer", "unattended-upgrades.service",
        "ufw.service", "nftables.service", "nats-server.service", "ssh.service", "ssh.socket",
        "serial-getty@ttyS0.service")
    for key in Path("/etc/ssh").glob("ssh_host_*"):
        key.unlink()
    run("systemctl", "set-default", "multi-user.target")


def verify_git(manifest, unit=BUILD / "guest/motor-tenant-agent.service", which=shutil.which):
    """The git that the guest runner and its jobs find first on the agent
    unit's PATH is the build server's release, not Debian's older package."""
    spec = manifest["tools"]["git"]
    search = next(line.removeprefix("Environment=PATH=") for line in Path(unit).read_text().splitlines()
                  if line.startswith("Environment=PATH="))
    path = which("git", path=search)
    if path != spec["path_by_role"]["tenant"]:
        raise ValueError(f"git resolves to {path}, not the pinned build")
    actual = run(path, "--version", capture=True)
    if actual != spec["version"]:
        raise ValueError(f"git version differs: {actual}")
    return actual


def verify_wasm_opt(manifest, unit=BUILD / "guest/motor-tenant-agent.service", which=shutil.which):
    spec = manifest["tools"]["wasm-opt"]
    search = next(line.removeprefix("Environment=PATH=") for line in Path(unit).read_text().splitlines()
                  if line.startswith("Environment=PATH="))
    path = which("wasm-opt", path=search)
    if path != spec["path_by_role"]["tenant"]:
        raise ValueError(f"wasm-opt resolves to {path}, not the pinned Binaryen release")
    actual = run(path, "--version", capture=True)
    expected_major = re.fullmatch(r"wasm-opt version (\d+)", spec["version"])
    actual_major = re.match(r"wasm-opt version (\d+)(?:\s|$)", actual)
    if not expected_major or not actual_major or expected_major.group(1) != actual_major.group(1):
        raise ValueError(f"wasm-opt major version differs: {actual}")
    return actual


def verify(pins):
    systemd = run("/usr/lib/systemd/systemd", "--version", capture=True)
    if int(systemd.split()[1]) < 257:
        raise ValueError("runner isolation requires systemd >=257 (PrivatePIDs)")
    inventory = {"systemd": systemd, "rust_toolchains": {}}
    inventory["agent_validated_paths"] = verify_agent_paths(Path("/"))
    inventory["static_module_load_requests"] = verify_no_module_load_requests(Path("/"))
    parity = json.loads(run("python3", str(PREFIX / "parity/toolchain-parity.py"),
                            "--check", "--role", "tenant", capture=True))
    inventory["parity"] = parity
    inventory["rust_toolchains"] = parity["observed"]["rust_toolchains"]
    inventory["runner"] = run("motor-runner", "--version", capture=True)
    manifest = json.loads((BUILD / "script/runner/toolchain.json").read_text())
    inventory["git"] = verify_git(manifest)
    inventory["wasm_opt"] = verify_wasm_opt(manifest)
    # Both errors occur during parsing, before a listener or runner can be opened.
    refuse_probe(["tenant-agent", "image-capability-probe"], "tenant-agent takes no arguments")
    refuse_probe(["image-capability-probe", "--tenant-vm", "--backend", "vm"],
                 "--tenant-vm requires the Linux host backend")
    inventory["sccache"] = run("sccache", "--version", capture=True)
    if inventory["sccache"] != "sccache 0.18.0":
        raise ValueError("expected the patched sccache 0.18.0")
    inventory["sccache_sha256"] = hashlib.sha256((PREFIX / "cargo/bin/sccache").read_bytes()).hexdigest()
    inventory["sccache_patch_sha256"] = pins["runner_inputs_sha256"][
        "script/runner/patches/sccache-0.18.0-read-only-hits.patch"]
    inventory["python"] = run("python3", "--version", capture=True)
    inventory["nextest"] = run("cargo", "nextest", "--version", capture=True)
    run("systemd-analyze", "verify", "/etc/systemd/system/motor-tenant-agent.service",
        "/etc/systemd/system/motor-net-rules.service", "/etc/systemd/system/motor-image-check.service")
    # Last, after every tool above has run and could have added files.
    inventory["world_readable_tool_trees"] = verify_tool_trees()
    inventory["root_owned_paths"] = root_paths.require(root_paths.TENANT_ROOTS)
    (BUILD / "tools.json").write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n")
    (BUILD / "packages.txt").write_text(run("dpkg-query", "-W", "-f=${Package}\t${Version}\n", capture=True) + "\n")
    print(f"guest inventory: {len(pins['packages'])} base packages; {len(inventory['rust_toolchains'])} Rust toolchains; "
          "4 JS tools; runner tenant-agent, git, sccache, nextest and Python executed")


def main():
    pins = json.loads((BUILD / "versions.json").read_text())
    install_tools(pins)
    configure(pins)
    verify(pins)
    run("apt-get", "clean")
    shutil.rmtree("/var/lib/apt/lists")
    for cache in ("/root/.npm", "/root/.cache"):
        if Path(cache).exists():
            shutil.rmtree(cache)
    # Do not seed every fresh guest from the same saved systemd random-seed file.
    Path("/var/lib/systemd/random-seed").unlink(missing_ok=True)


if __name__ == "__main__":
    main()
