#!/usr/bin/python3
"""Runs inside the disposable chroot, using only its staged inputs and public downloads."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import urllib.request

BUILD = Path("/srv/build")
PREFIX = Path("/usr/local/lib/motor")
GUEST_SERVICES = ("motor-tenant-agent.service", "motor-net-rules.service", "motor-image-check.service")
MODULE_LOAD_DIRS = ("etc/modules-load.d", "run/modules-load.d", "usr/local/lib/modules-load.d",
                    "usr/lib/modules-load.d", "lib/modules-load.d")


def run(*args, capture=False) -> str:
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def refuse_probe(args, expected):
    result = subprocess.run(["motor-runner", *args], text=True, capture_output=True)
    if result.returncode == 0 or expected not in result.stderr:
        raise ValueError(f"runner capability probe failed: {args}: {result.stderr}")


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
    os.environ.update(
        CARGO_HOME="/srv/build/cargo-home", RUSTUP_HOME="/home/ubuntu/.rustup",
        CARGO_TARGET_DIR="/srv/build/target", TMPDIR="/srv/build/tmp",
        RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", GIT_TERMINAL_PROMPT="0",
        HOME="/root", PATH="/srv/build/cargo-home/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
    )
    (BUILD / "tmp").mkdir()
    rustup_url = (f"https://static.rust-lang.org/rustup/archive/{pins['rustup']}/"
                  "x86_64-unknown-linux-gnu/rustup-init")
    rustup_sha = urllib.request.urlopen(rustup_url + ".sha256").read().decode().split()[0]
    download(rustup_url, BUILD / "rustup-init", rustup_sha)
    (BUILD / "rustup-init").chmod(0o755)
    run(str(BUILD / "rustup-init"), "-y", "--no-modify-path", "--profile", "minimal",
        "--default-toolchain", pins["rust_default"])
    for toolchain, targets in pins["rust_toolchains"].items():
        run("rustup", "toolchain", "install", toolchain, "--profile", "minimal",
            "--component", "clippy", "--component", "rustfmt")
        run("rustup", "target", "add", "--toolchain", toolchain, *targets)
    run("rustup", "default", pins["rust_default"])

    node_file = f"node-v{pins['node']}-linux-x64.tar.xz"
    node_url = f"https://nodejs.org/dist/v{pins['node']}/"
    download(node_url + node_file, BUILD / node_file,
             published_hash(node_url + "SHASUMS256.txt", node_file))
    run("tar", "-xJf", str(BUILD / node_file), "--strip-components=1", "-C", "/usr/local")
    bun_file = "bun-linux-x64.zip"
    bun_url = f"https://github.com/oven-sh/bun/releases/download/bun-v{pins['bun']}/"
    download(bun_url + bun_file, BUILD / bun_file,
             published_hash(bun_url + "SHASUMS256.txt", bun_file))
    run("unzip", "-q", str(BUILD / bun_file), "-d", str(BUILD / "bun"))
    shutil.copy2(BUILD / "bun/bun-linux-x64/bun", "/usr/local/bin/bun")
    Path("/usr/local/bin/bun").chmod(0o755)
    run("npm", "install", "--global", "--prefix", "/usr/local", "--ignore-scripts",
        f"npm@{pins['npm']}", f"pnpm@{pins['pnpm']}")
    run("bash", str(BUILD / "script/runner/install-tools.sh"))
    run("sh", str(BUILD / "script/runner/build-sccache.sh"))

    PREFIX.mkdir(parents=True)
    (PREFIX / "cargo").mkdir()
    shutil.copytree(BUILD / "cargo-home/bin", PREFIX / "cargo/bin", symlinks=True)
    shutil.move("/home/ubuntu/.rustup", PREFIX / "rustup")
    Path("/home/ubuntu").rmdir()
    # Only immutable binaries and toolchains enter the image, not install caches.
    os.environ.update(CARGO_HOME=str(PREFIX / "cargo"), RUSTUP_HOME=str(PREFIX / "rustup"),
                      PATH=f"{PREFIX}/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")


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


def configure(pins):
    for account in ("motor-job", "motor-cache"):
        run("useradd", "--system", "--user-group", "--home-dir", "/nonexistent",
            "--shell", "/usr/sbin/nologin", account)
    for directory in ("tenant", "Users", "home", "overlay", ".lower", "usr/local/libexec", "etc/nftables.d"):
        Path("/", directory).mkdir(parents=True, exist_ok=True)
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
        "ufw.service", "nftables.service", "nats-server.service", "serial-getty@ttyS0.service")
    run("systemctl", "set-default", "multi-user.target")


def verify(pins):
    systemd = run("/usr/lib/systemd/systemd", "--version", capture=True)
    if int(systemd.split()[1]) < 257:
        raise ValueError("runner isolation requires systemd >=257 (PrivatePIDs)")
    inventory = {"systemd": systemd, "rust_toolchains": {}}
    inventory["static_module_load_requests"] = verify_no_module_load_requests(Path("/"))
    for name, command in (("node", "node"), ("npm", "npm"), ("bun", "bun"), ("pnpm", "pnpm")):
        actual = run(command, "--version", capture=True).removeprefix("v")
        if actual != pins[name]:
            raise ValueError(f"{name} version differs: {actual}")
        inventory[name] = actual
    for toolchain, expected in pins["rust_toolchains"].items():
        actual = run("rustup", "target", "list", "--installed", "--toolchain", toolchain, capture=True).splitlines()
        if sorted(actual) != sorted(expected):
            raise ValueError(f"Rust targets differ: {toolchain}")
        inventory["rust_toolchains"][toolchain] = actual
    inventory["runner"] = run("motor-runner", "--version", capture=True)
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
    (BUILD / "tools.json").write_text(json.dumps(inventory, indent=2, sort_keys=True) + "\n")
    (BUILD / "packages.txt").write_text(run("dpkg-query", "-W", "-f=${Package}\t${Version}\n", capture=True) + "\n")
    print(f"guest inventory: {len(pins['packages'])} packages; {len(pins['rust_toolchains'])} Rust toolchains; "
          "4 JS tools; runner tenant-agent, sccache, nextest and Python executed")


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
