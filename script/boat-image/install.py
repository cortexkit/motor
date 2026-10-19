#!/usr/bin/env python3
"""Root-only, credential-free Boat runner host installation."""

import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
BUILD = Path("/srv/build")
STATE = Path("/var/lib/motor-boat")
APT_LISTS = Path('/var/lib/apt/lists/motor-boat')
APT_OPTIONS = ['-o', 'Dir::Etc::sourcelist=/dev/null', '-o', 'Dir::Etc::sourceparts=/etc/apt/motor-boat.sources.d',
               '-o', 'Dir::State::lists=' + str(APT_LISTS), '-o', 'Acquire::AllowInsecureRepositories=false',
               '-o', 'APT::Get::AllowUnauthenticated=false']


def run(*args, capture=False):
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def digest(path, algorithm="sha256"):
    hashed = hashlib.new(algorithm)
    with Path(path).open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            hashed.update(chunk)
    return hashed.hexdigest()


def checksum(text, filename):
    matches = [line.split()[0] for line in text.splitlines()
               if len(line.split()) == 2 and line.split()[1].lstrip("*") == filename]
    if len(matches) != 1 or not re.fullmatch(r"[a-f0-9]{64}", matches[0]):
        raise ValueError(f"missing or ambiguous published SHA-256: {filename}")
    return matches[0]


def download(url, destination, expected):
    if not re.fullmatch(r"[a-f0-9]{64}", expected):
        raise ValueError("invalid SHA-256")
    urllib.request.urlretrieve(url, destination)
    if digest(destination) != expected:
        raise ValueError(f"SHA-256 mismatch: {url}")


def published(url, filename, sums="SHASUMS256.txt"):
    text = urllib.request.urlopen(url + sums, timeout=60).read().decode()
    path = BUILD / "downloads" / filename
    download(url + filename, path, checksum(text, filename))
    return path


def apt_update(attempts=3, pause=time.sleep):
    for attempt in range(attempts):
        result = subprocess.run(["apt-get", "update", *APT_OPTIONS, "-o", "APT::Update::Error-Mode=any"],
                                capture_output=True, text=True)
        print(result.stdout, end="", flush=True)
        print(result.stderr, end="", file=sys.stderr, flush=True)
        if result.returncode == 0:
            return
        transient = bool(re.search(r"\b50[0234]\b", result.stdout + result.stderr))
        if not transient or attempt + 1 == attempts:
            raise RuntimeError("Ubuntu package index refresh failed; refusing package installation or cached-index fallback")
        pause(2 ** attempt)


def archive_preflight(pins, fetch=urllib.request.urlopen, pause=time.sleep):
    urls = [f"{pins['ubuntu_archive']}/dists/{suite}/InRelease" for suite in ('noble', 'noble-updates')]
    urls.append(f"{pins['ubuntu_security']}/dists/noble-security/InRelease")
    evidence = []
    for url in urls:
        record = {"url": url, "attempts": []}
        evidence.append(record)
        for attempt in range(3):
            try:
                with fetch(url, timeout=12) as response:
                    body = response.read()
                    record["attempts"].append({"status": response.status, "bytes": len(body),
                        "sha256": hashlib.sha256(body).hexdigest(),
                        "pgp_signed": body.startswith(b"-----BEGIN PGP SIGNED MESSAGE-----")})
                break
            except urllib.error.HTTPError as error:
                record["attempts"].append({"status": error.code})
                error.close()
                if error.code < 500 or attempt == 2:
                    break
                pause(2 ** attempt)
            except OSError as error:
                record["attempts"].append({"error": str(error)})
                break
    return evidence


def release_manifest(directory=APT_LISTS):
    records = {}
    for path in directory.glob('*_InRelease'):
        text = path.read_text()
        dates = re.findall(r'(?m)^Date: (.+)$', text)
        if len(dates) != 1 or not text.startswith('-----BEGIN PGP SIGNED MESSAGE-----'):
            raise ValueError('missing signed Ubuntu Release Date: ' + path.name)
        records[path.name] = {'date': dates[0], 'sha256': digest(path)}
    if len(records) != 3:
        raise ValueError('expected exactly three official Ubuntu Release indexes')
    return records


def validate(pins):
    if pins['ubuntu_archive'] != 'https://archive.ubuntu.com/ubuntu' or pins['ubuntu_security'] != 'https://security.ubuntu.com/ubuntu':
        raise ValueError('only official signed Ubuntu archive sources are allowed')
    if pins["probe_port"] == 9443 or not 1024 <= pins["probe_port"] <= 65535:
        raise ValueError("probe bridge must not claim the endpoint port")
    for key in ("rustup", "rust_default", "node", "bun", "nats", "pnpm"):
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", pins[key]):
            raise ValueError(f"unpinned version: {key}")
    if not {"1.99.0", "1.89.0"} <= set(pins["rust_toolchains"]):
        raise ValueError("missing fleet Rust toolchains")
    for name, expected in pins["runner_inputs_sha256"].items():
        if digest(ROOT / name) != expected:
            raise ValueError(f"runner input changed without refreshing its pin: {name}")


def state(pins):
    image = Path("/var/lib/motor-boat.xfs")
    STATE.mkdir(parents=True, exist_ok=True)
    if not image.exists():
        # Sparse capacity avoids padding the provider's disk snapshot with zeros.
        with image.open("xb") as stream:
            stream.truncate(pins["state_gib"] * 1024**3)
        run("mkfs.xfs", "-q", "-m", "reflink=1", str(image))
    mount = Path("/etc/systemd/system/var-lib-motor\\x2dboat.mount")
    mount.write_text("[Unit]\nDescription=Boat reflink runner disk\n[Mount]\n"
                     "What=/var/lib/motor-boat.xfs\nWhere=/var/lib/motor-boat\n"
                     "Type=xfs\nOptions=loop,noatime\n[Install]\nWantedBy=multi-user.target\n")
    run("systemctl", "daemon-reload")
    run("systemctl", "enable", "--now", mount.name)
    if run("findmnt", "-n", "-o", "FSTYPE", str(STATE), capture=True) != "xfs":
        raise ValueError("runner disk is not XFS")
    for directory in ("state", "state/workspaces", "home", "tmp"):
        (STATE / directory).mkdir(parents=True, exist_ok=True)
        run("chown", "root:root", str(STATE / directory))
    run("chown", "root:root", str(STATE))
    Path("/Users").mkdir(exist_ok=True)
    Path("/etc/systemd/system/Users.mount").write_text(
        "[Unit]\nRequires=var-lib-motor\\x2dboat.mount\nAfter=var-lib-motor\\x2dboat.mount\n"
        "[Mount]\nWhat=/var/lib/motor-boat/state/workspaces\nWhere=/Users\nType=none\n"
        "Options=bind\n[Install]\nWantedBy=multi-user.target\n")
    run("systemctl", "daemon-reload")
    run("systemctl", "enable", "--now", "Users.mount")
    with tempfile.NamedTemporaryFile(dir=STATE) as source, tempfile.NamedTemporaryFile(dir="/Users") as target:
        source.write(os.urandom(4096))
        source.flush()
        fcntl.ioctl(target.fileno(), 0x40049409, source.fileno())
        if Path(source.name).read_bytes() != Path(target.name).read_bytes():
            raise ValueError("FICLONE content mismatch")


def tools(pins):
    (BUILD / "downloads").mkdir(parents=True, exist_ok=True)
    (BUILD / "tmp").mkdir(exist_ok=True)
    os.environ.update(CARGO_HOME="/srv/build/cargo-home", RUSTUP_HOME="/home/ubuntu/.rustup",
                      TMPDIR="/srv/build/tmp", CARGO_TARGET_DIR="/srv/build/target",
                      RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", GIT_TERMINAL_PROMPT="0",
                      CARGO_BUILD_JOBS="4",
                      PATH="/srv/build/cargo-home/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
    rustup = (f"https://static.rust-lang.org/rustup/archive/{pins['rustup']}/"
              "x86_64-unknown-linux-gnu/rustup-init")
    path = BUILD / "downloads/rustup-init"
    download(rustup, path, urllib.request.urlopen(rustup + ".sha256", timeout=60).read().decode().split()[0])
    path.chmod(0o755)
    run(str(path), "-y", "--no-modify-path", "--profile", "minimal", "--default-toolchain", pins["rust_default"])
    for toolchain in pins["rust_toolchains"]:
        run("rustup", "toolchain", "install", toolchain, "--profile", "minimal",
            "--component", "clippy", "--component", "rustfmt")
    run("rustup", "default", pins["rust_default"])
    node = published(f"https://nodejs.org/dist/v{pins['node']}/", f"node-v{pins['node']}-linux-x64.tar.xz")
    run("tar", "-xJf", str(node), "--strip-components=1", "-C", "/usr/local")
    metadata = json.loads(urllib.request.urlopen(f"https://registry.npmjs.org/pnpm/{pins['pnpm']}", timeout=60).read())
    algorithm, encoded = metadata["dist"]["integrity"].split("-", 1)
    if algorithm != "sha512":
        raise ValueError("pnpm registry integrity is not SHA-512")
    pnpm = BUILD / "downloads/pnpm.tgz"
    urllib.request.urlretrieve(metadata["dist"]["tarball"], pnpm)
    if bytes.fromhex(digest(pnpm, "sha512")) != base64.b64decode(encoded, validate=True):
        raise ValueError("pnpm integrity mismatch")
    run("npm", "install", "--global", "--prefix", "/usr/local", "--ignore-scripts", str(pnpm))
    bun = published(f"https://github.com/oven-sh/bun/releases/download/bun-v{pins['bun']}/", "bun-linux-x64.zip")
    run("unzip", "-qo", str(bun), "-d", str(BUILD / "downloads"))
    shutil.copy2(BUILD / "downloads/bun-linux-x64/bun", "/usr/local/bin/bun")
    Path("/usr/local/bin/bun").chmod(0o755)
    nats = published(f"https://github.com/nats-io/nats-server/releases/download/v{pins['nats']}/",
                     f"nats-server-v{pins['nats']}-linux-amd64.tar.gz", "SHA256SUMS")
    run("tar", "-xzf", str(nats), "-C", str(BUILD / "downloads"))
    run("install", "-m", "0755", str(BUILD / f"downloads/nats-server-v{pins['nats']}-linux-amd64/nats-server"),
        "/usr/local/bin/nats-server")
    # Fail anonymously instead of forwarding an operator's repository credentials.
    run("git", "-c", "credential.helper=", "ls-remote", "https://github.com/cortexkit/commons", "HEAD")
    run("bash", str(ROOT / "script/runner/install-tools.sh"))
    marker = BUILD / "sccache-inputs.sha256"
    inputs = digest(ROOT / "script/runner/build-sccache.sh") + digest(
        ROOT / "script/runner/patches/sccache-0.18.0-read-only-hits.patch")
    if not marker.exists() or marker.read_text() != inputs:
        run("sh", str(ROOT / "script/runner/build-sccache.sh"))
        marker.write_text(inputs)


def target_directories():
    run('install', '-d', '-o', 'root', '-g', 'root', '-m', '0755',
        '/usr/local/bin', '/usr/local/libexec', '/etc/systemd/system',
        '/etc/ssh/sshd_config.d', '/etc/motor', '/etc/motor/tenant')


def configure(pins):
    target_directories()
    for account in ("motor-job", "motor-cache"):
        if subprocess.run(["id", "-u", account], capture_output=True).returncode:
            run("useradd", "--system", "--user-group", "--home-dir", "/nonexistent",
                "--shell", "/usr/sbin/nologin", account)
    state(pins)
    config = Path("/etc/ssh/sshd_config").read_text()
    if not re.search(r"(?im)^\s*Include\s+/etc/ssh/sshd_config.d/\*\.conf\s*$", config):
        raise ValueError("sshd does not include drop-ins; refusing an unverified policy")
    Path("/etc/ssh/sshd_config.d/00-motor-boat.conf").write_text(
        "AllowTcpForwarding local\nPermitTTY no\nAllowAgentForwarding no\nX11Forwarding no\nPermitTunnel no\n")
    run("/usr/sbin/sshd", "-t")
    run("systemctl", "reload", "ssh")
    for name in ("motor-boat-network", "motor-boat-bridge.py", "motor-boat-start.py"):
        run("install", "-m", "0755", str(HERE / name), "/usr/local/libexec/" + name)
    run("install", "-m", "0644", str(ROOT / "script/runner/motor-net.nft"),
        "/usr/local/libexec/motor-boat-net.nft")
    cpus = int(run("nproc", capture=True))
    for name in ("motor-boat.service", "motor-boat-bridge.service", "motor-boat-host.service", "motor-boat-host.path"):
        Path("/etc/systemd/system", name).write_text((HERE / name).read_text().replace("@VCPUS@", str(cpus)))
    (STATE / 'host-journal').mkdir(mode=0o700, exist_ok=True)
    Path('/etc/motor/sandbox.json').write_text(json.dumps({
        'journal_dir': str(STATE / 'host-journal'),
        'job_socket': '/run/motor-tenant/jobs.sock',
        'event_socket': '/run/motor-tenant/events.sock',
        'admin_socket': '/run/motor-tenant/admin.sock',
        'vcpu_ceiling': cpus,
    }, indent=2) + '\n')
    Path("/etc/motor/tenant").mkdir(parents=True, exist_ok=True)
    Path("/etc/motor/tenant/id").unlink(missing_ok=True)
    run("systemctl", "daemon-reload")
    run("systemd-analyze", "verify", "/etc/systemd/system/motor-boat.service",
        "/etc/systemd/system/motor-boat-bridge.service", "/etc/systemd/system/motor-boat-host.service",
        "/etc/systemd/system/motor-boat-host.path")
    run("systemctl", "enable", "motor-boat.service", "motor-boat-host.path")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", help="exact clean source commit to build")
    parser.add_argument("--apt-preflight-only", action="store_true", help="record official archive index reachability without changing the guest")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("apply as root")
    pins = json.loads((HERE / "pins.json").read_text())
    validate(pins)
    if args.apt_preflight_only:
        print(json.dumps(archive_preflight(pins)))
        return
    if not args.revision:
        parser.error("installation requires --revision")
    if run("git", "-C", str(ROOT), "rev-parse", "HEAD", capture=True) != args.revision:
        raise ValueError("source revision differs")
    if run("git", "-C", str(ROOT), "status", "--porcelain", "--untracked-files=no", capture=True):
        raise ValueError("source has modified tracked files")
    release = dict(line.split("=", 1) for line in Path("/etc/os-release").read_text().splitlines() if "=" in line)
    if release.get("VERSION_ID", "").strip('"') != pins["ubuntu"]:
        raise ValueError("recipe requires pinned Ubuntu " + pins["ubuntu"])
    manager = run("systemctl", "show", "--property=Version", "--value", capture=True)
    if int(manager.split(".")[0]) < pins["systemd_min"]:
        raise ValueError("unsupported systemd manager")
    # An isolated source/list directory excludes Boat's third-party repositories.
    sources = Path('/etc/apt/motor-boat.sources.d')
    sources.mkdir(exist_ok=True)
    (sources / 'ubuntu.sources').write_text(
        f"Types: deb\nURIs: {pins['ubuntu_archive']}\nSuites: noble noble-updates\n"
        "Components: main universe restricted multiverse\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n\n"
        f"Types: deb\nURIs: {pins['ubuntu_security']}\nSuites: noble-security\n"
        "Components: main universe restricted multiverse\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n")
    os.environ["DEBIAN_FRONTEND"] = "noninteractive"
    (APT_LISTS / 'partial').mkdir(parents=True, exist_ok=True)
    apt_update()
    releases = release_manifest()
    # Keep provider-managed init and SSH binaries; only our SSH drop-in is installed.
    run("apt-mark", "hold", "systemd", "systemd-sysv", "libsystemd-shared", "openssh-server")
    run("apt-get", "install", *APT_OPTIONS, "-y", "--no-install-recommends", *pins["packages"])
    target_directories()
    tools(pins)
    os.chdir(ROOT)
    run("cargo", "build", "--release", "--locked", "-p", "motor-runner", "-p", "motor-host")
    for binary in ('motor-runner', 'motor-host'):
        run("install", "-m", "0755", '/srv/build/target/release/' + binary, '/usr/local/bin/' + binary)
    configure(pins)
    inventory: dict[str, object] = {name: run(*command, capture=True) for name, command in {
        "runner": ["motor-runner", "--version"], "rust": ["rustc", "--version"],
        "node": ["node", "--version"], "bun": ["bun", "--version"],
        "pnpm": ["pnpm", "--version"],
        "nats": ["nats-server", "--version"], "python": ["python3", "--version"],
        "sccache": ["sccache", "--version"], "systemd": ["systemctl", "--version"],
    }.items()}
    inventory.update(revision=args.revision, runner_sha256=digest("/usr/local/bin/motor-runner"),
                     packages=run("dpkg-query", "-W", capture=True), apt_release_files=releases)
    for key, expected in {"node": "v" + pins["node"], "bun": pins["bun"], "pnpm": pins["pnpm"],
                          "nats": "nats-server: v" + pins["nats"], "sccache": "sccache 0.18.0"}.items():
        if inventory[key] != expected:
            raise ValueError(f"installed {key} differs from pin: {inventory[key]}")
    (STATE / "inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
    print(json.dumps(inventory))


if __name__ == "__main__":
    main()
