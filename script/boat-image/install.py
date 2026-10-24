#!/usr/bin/env python3
"""Root-only Boat fleet host installation; private Cargo seed is tenant-1-only."""

import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import stat
import subprocess
import sys
import sysconfig
import tempfile
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
PARITY_PATH = ROOT / "script/runner/toolchain.json"
parity_spec = importlib.util.spec_from_file_location("toolchain_parity", ROOT / "script/runner/toolchain-parity.py")
if parity_spec is None or parity_spec.loader is None:
    raise ImportError("cannot load toolchain parity checker")
parity = importlib.util.module_from_spec(parity_spec)
parity_spec.loader.exec_module(parity)
account_spec = importlib.util.spec_from_file_location("job_account", ROOT / "script/runner/job-account.py")
if account_spec is None or account_spec.loader is None:
    raise ImportError("cannot load the shared job identity installer")
job_account = importlib.util.module_from_spec(account_spec)
account_spec.loader.exec_module(job_account)
# The tenant image recipe owns the shared root-owned path check; Boat loads the
# same file from the source checkout so both images enforce one invariant.
root_paths_spec = importlib.util.spec_from_file_location("root_paths", ROOT / "script/tenant-image/guest/root_paths.py")
if root_paths_spec is None or root_paths_spec.loader is None:
    raise ImportError("cannot load the root-owned path check")
root_paths = importlib.util.module_from_spec(root_paths_spec)
root_paths_spec.loader.exec_module(root_paths)
BUILD = Path("/srv/build")
STATE = Path("/var/lib/motor-boat")
# The runner's admission floor, the --free-space-floor-bytes value in
# motor-boat.service. The endpoint withholds readiness below it.
# Leave the provisional 24-GiB reserve, 64-MiB replay tail and an 8-GiB cold cap.
FREE_SPACE_FLOOR_BYTES = (32 << 30) + (64 << 20)
APT_LISTS = Path('/var/lib/apt/lists/motor-boat')
APT_OPTIONS = ['-o', 'Dir::Etc::sourcelist=/dev/null', '-o', 'Dir::Etc::sourceparts=/etc/apt/motor-boat.sources.d',
               '-o', 'Dir::State::lists=' + str(APT_LISTS), '-o', 'Acquire::AllowInsecureRepositories=false',
               '-o', 'APT::Get::AllowUnauthenticated=false']
RUNNER_INPUTS = (
    'script/runner/tools.txt', 'script/runner/install-tools.sh', 'script/runner/install-npm-tool.py',
    'script/runner/job-account.py',
    'script/runner/units/system/motor-jobs.slice',
    'script/runner/sysctl/60-motor-inotify.conf',
    'script/runner/build-sccache.sh', 'script/runner/patches/sccache-0.18.0-read-only-hits.patch',
    'script/runner/build-git.sh', 'script/runner/install-binaryen.py',
) + tuple(f'script/runner/{name}' for name in parity.PARITY_FILES)
# motor-boat.service sets HOME, RUSTUP_HOME and the first PATH entry beneath it.
TOOL_HOME = Path("/home/ubuntu")
# Boat's base ships /usr/local as uid 1000: the provider's sandbox account,
# which Boat's exec API runs as and which has passwordless sudo. Root runs the
# runner, the endpoint and the job PID namespace init from /usr/local, so every
# path there must be root's (root_paths.py). The recipe settles that provider
# layout before the tool installers write under /usr/local, by one rule rather
# than a list of the provider's trees: Ubuntu's skeleton directories stay and
# become root's, and every other entry the sandbox account owns there is
# removed. Boat's base carries toolchains the image never uses this way (Go,
# rustup and Cargo trees, a Ruby install under include/, lib/ and bin/); the
# build server has none of them, and jobs mount /usr read-only, so removing
# them keeps them out of job view as on the build server. The runner, the
# endpoint and jobs take Rust from RUSTUP_HOME=/home/ubuntu/.rustup and
# CARGO_HOME=/srv/build/cargo-home (jobs: /motor-home/cargo), and the
# toolchain manifest names no Go or Ruby. An entry owned by any account other
# than root or BOAT_BASE_UID refuses the build before anything changes. Root's
# own entries stay where they are, except inside a removed provider directory
# or as a PATH symlink into one, so a remaining root-owned entry that is
# unsafe fails the root-owned path check by name.
BOAT_BASE_UID = 1000
# Packages that Boat's base image supplies but the runner does not use. `at`
# leaves a daemon-owned setuid executable; Binaryen is replaced by the pinned
# release because Ubuntu 24.04's version cannot read current wasm-bindgen output.
BOAT_BASE_PURGE = ("at", "binaryen")
BOAT_BASE_USR_LOCAL = {
    # The tree walked: every entry beneath it, at any depth, without following
    # symlinks, so a link never leads the walk (or a removal) outside it.
    "root": "/usr/local",
    # Directories that stay and receive our installs, each re-owned root:root
    # with group/other write cleared, never recursively: /usr/local and Ubuntu
    # base-files' skeleton beneath it (man is its link to share/man), plus the
    # directories the image's python3 and ca-certificates packages create there.
    # Every other entry the sandbox account owns under "root" is removed.
    "skeleton": ("/usr/local", "/usr/local/bin", "/usr/local/etc", "/usr/local/games", "/usr/local/include",
                 "/usr/local/lib", "/usr/local/man", "/usr/local/sbin", "/usr/local/share", "/usr/local/share/man",
                 "/usr/local/src", "/usr/local/lib/python3.12", "/usr/local/lib/python3.12/dist-packages",
                 "/usr/local/share/ca-certificates"),
    # Directories searched for a command name: motor-boat.service's PATH and
    # systemd's default PATH, which motor-boat-host.service inherits. A symlink
    # directly in one of them that points into a removed entry (a root-owned
    # /usr/bin/ruby leading to the provider's /usr/local/lib/ruby, say) is
    # removed too, so nothing root or a job runs by name still leads there.
    "search_path": ("/home/ubuntu/.bun/bin", "/srv/build/cargo-home/bin", "/usr/local/sbin", "/usr/local/bin",
                    "/usr/sbin", "/usr/bin", "/sbin", "/bin"),
}
# How many path levels below BOAT_BASE_USR_LOCAL["root"] the inventory groups
# removed entries by: /usr/local/include/ruby-4.0.0 with its entry count, not
# every file inside it.
REMOVED_GROUP_LEVELS = 2


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


def account_processes(uid, proc=Path("/proc")):
    """PIDs whose real, effective, saved or filesystem uid is `uid`."""
    pids = []
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            status = (entry / "status").read_text()
        except OSError:
            continue
        uids = next((line.split()[1:] for line in status.splitlines() if line.startswith("Uid:")), [])
        if str(uid) in uids:
            pids.append(int(entry.name))
    return sorted(pids)


def tool_home(home=TOOL_HOME, proc=Path("/proc")):
    """The runner's Bun PATH entry and its RUSTUP_HOME are under
    /home/ubuntu, so root runs Bun and the Rust toolchains from beneath it and
    the directory must be root's. Create it root:root 0755 when absent. Refuse
    one another account owns rather than take over a provider account's home
    without anyone deciding to."""
    try:
        metadata = home.lstat()
    except FileNotFoundError:
        run("install", "-d", "-o", "root", "-g", "root", "-m", "0755", str(home))
        return {"path": str(home), "created": True}
    if not stat.S_ISDIR(metadata.st_mode):
        raise ValueError(f"{home} must be a physical directory; root executes tools from beneath it")
    if metadata.st_uid != 0:
        try:
            account = pwd.getpwuid(metadata.st_uid)
            entry = ":".join(str(field) for field in (account.pw_name, "x", account.pw_uid, account.pw_gid,
                                                     account.pw_gecos, account.pw_dir, account.pw_shell))
        except KeyError:
            entry = "no passwd entry"
        processes = account_processes(metadata.st_uid, proc)
        raise ValueError(
            f"{home} is owned by uid {metadata.st_uid} ({entry}); running processes: {processes or 'none'}. "
            "Root executes the runner's Bun and Rust toolchains from beneath it, so it must be root-owned; "
            "decide whether that account may be retired before reinstalling")
    if metadata.st_mode & 0o022:
        raise ValueError(f"{home} has mode {stat.S_IMODE(metadata.st_mode):04o}; group or other may write "
                         "where root executes the runner's tools")
    return {"path": str(home), "created": False}


def running_paths():
    """What this recipe runs from: its interpreter, that interpreter's standard
    library and the source checkout. README runs the recipe with /usr/bin/python3;
    a bare `sudo python3` could instead find a provider interpreter under
    /usr/local/bin, which removing the provider's entries would delete mid-run."""
    return {Path(os.path.realpath(sys.executable)), Path(os.path.realpath(sysconfig.get_paths()["stdlib"])), ROOT}


def boat_base_layout(image=Path("/"), layout=BOAT_BASE_USR_LOCAL, provider_uid=BOAT_BASE_UID, chown=os.lchown,
                     protected=None):
    """Apply BOAT_BASE_USR_LOCAL to the image at `image` ("/" when building, a
    fixture directory in tests) and return what changed. The whole tree is
    walked and every owner checked before anything changes, so a refusal
    leaves the image as it was. `protected` paths (default: running_paths())
    must not lie inside anything removed. Idempotent: a reapplied recipe finds
    the provider's entries gone and the skeleton already root's."""
    image = Path(os.path.realpath(image))
    protected = running_paths() if protected is None else set(protected)

    def inside(name):
        return image / name.lstrip("/")

    def name_of(path):
        return "/" + str(path.relative_to(image))

    def provider_or_root(name, metadata, action):
        if metadata.st_uid not in (0, provider_uid):
            raise ValueError(f"refusing to {action} {name}: owned by uid {metadata.st_uid}, "
                             f"neither root nor Boat's base account uid {provider_uid}")
        return {"path": name, "uid": metadata.st_uid, "gid": metadata.st_gid}

    report = {"provider_uid": provider_uid, "removed": {}, "removed_entries": 0, "unlinked": [], "reowned": [],
              "absent": []}
    top = inside(layout["root"])
    skeleton = {inside(name) for name in layout["skeleton"]}
    # removals: the topmost removed entries, each with how many entries its
    # removal takes (itself and everything beneath it).
    removals, links, directories, seen = [], [], [], set()

    def visit(path, metadata, removed_above):
        """Classify one entry and walk beneath it; return its entry count.
        lstat reports a symlink as a link, never as the directory it may point
        at, so links are counted and never descended."""
        name = name_of(path)
        action = "re-own" if path in skeleton else "remove" if removed_above else "leave in place"
        record = provider_or_root(name, metadata, action)
        removing = False
        if path in skeleton:
            seen.add(path)
            directories.append((path, metadata, record))
        elif not removed_above and metadata.st_uid == provider_uid:
            removing = True
        count = 1
        if stat.S_ISDIR(metadata.st_mode):
            for entry in sorted(path.iterdir()):
                count += visit(entry, entry.lstat(), removed_above or removing)
        if removing:
            removals.append((path, metadata, count))
        return count

    try:
        top_metadata = top.lstat()
    except FileNotFoundError:
        pass
    else:
        visit(top, top_metadata, False)
    report["absent"] = [name for name in layout["skeleton"] if inside(name) not in seen]
    removed = {path for path, _, _ in removals}
    for path in sorted(protected):
        if any(candidate in removed for candidate in (path, *path.parents)):
            raise ValueError(f"refusing to remove Boat's /usr/local entries: {path} is among them and this recipe "
                             "runs from it; run the recipe with /usr/bin/python3 from a checkout outside /usr/local")
    # An absolute link target is read beneath `image` and a relative one from
    # the link's directory, both normalized without following any link, so a
    # fixture's absolute links never reach the running system's files. A link
    # that is itself removed above is not listed again.
    searched = set()
    for name in layout["search_path"]:
        directory = Path(os.path.realpath(inside(name)))
        if directory in searched or not directory.is_dir():
            continue
        searched.add(directory)
        for entry in sorted(directory.iterdir()):
            if not entry.is_symlink() or entry in removed:
                continue
            target = os.readlink(entry)
            lexical = Path(os.path.normpath(inside(target) if os.path.isabs(target) else entry.parent / target))
            if any(candidate in removed for candidate in (lexical, *lexical.parents)):
                record = provider_or_root(name_of(entry), entry.lstat(), "unlink")
                links.append((entry, dict(record, target=target)))

    for entry, record in links:
        entry.unlink()
        report["unlinked"].append(record)
    groups = {}
    for path, metadata, count in sorted(removals, key=lambda row: row[0]):
        if stat.S_ISDIR(metadata.st_mode):
            shutil.rmtree(path)
        else:
            path.unlink()
        group = name_of(top.joinpath(*path.relative_to(top).parts[:REMOVED_GROUP_LEVELS]))
        groups[group] = groups.get(group, 0) + count
        report["removed_entries"] += count
    report["removed"] = groups
    for path, metadata, record in directories:
        mode = stat.S_IMODE(metadata.st_mode)
        changed = False
        if (metadata.st_uid, metadata.st_gid) != (0, 0):
            chown(path, 0, 0)
            changed = True
        # A symlink's own mode bits are meaningless on Linux.
        if not stat.S_ISLNK(metadata.st_mode) and mode & 0o022:
            path.chmod(mode & ~0o022)
            changed = True
        if changed:
            report["reowned"].append(dict(record, mode=f"{mode:04o}"))
    return report


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


def apt_install(packages):
    # Boat holds provider-managed packages such as sshd. Keep those installed
    # versions rather than asking apt to upgrade them or overriding the holds.
    held = set(run("apt-mark", "showhold", capture=True).splitlines())
    skipped = {}
    for package in packages:
        if package not in held:
            continue
        try:
            status = run("dpkg-query", "-W", "-f=${db:Status-Status}\t${Version}", package, capture=True)
        except subprocess.CalledProcessError as error:
            raise RuntimeError(f"required held package {package} is not installed; refusing to change its hold") from error
        state, _, version = status.partition("\t")
        if state != "installed" or not version:
            raise RuntimeError(f"required held package {package} is not installed; refusing to change its hold")
        skipped[package] = version
    requested = [package for package in packages if package not in held]
    if requested:
        run("apt-get", "install", *APT_OPTIONS, "-y", "--no-install-recommends", *requested)
    return skipped


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
    manifest = json.loads(PARITY_PATH.read_text())
    parity.validate(manifest)
    if set(pins['runner_inputs_sha256']) != set(RUNNER_INPUTS):
        raise ValueError('runner input pins must cover the parity manifest and complete installer inputs')
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
    # Storage preparation starts these units explicitly after restore cleanup.
    # Neither unit may be pulled in independently during boot or remount live
    # state after the storage evidence file blocks further mount activation.
    mount.write_text("[Unit]\nDescription=Boat reflink runner disk\n"
                     "DefaultDependencies=no\nConflicts=umount.target\nBefore=umount.target\nRequires=motor-boat-restored.service\n"
                     "After=motor-boat-restored.service\nConditionPathExists=!/run/motor-boat-storage.json\n[Mount]\n"
                     "What=/var/lib/motor-boat.xfs\nWhere=/var/lib/motor-boat\n"
                     "Type=xfs\nOptions=loop,noatime,prjquota\n")
    Path("/Users").mkdir(exist_ok=True)
    Path("/etc/systemd/system/Users.mount").write_text(
        "[Unit]\nDefaultDependencies=no\nConflicts=umount.target\nBefore=umount.target\nRequires=var-lib-motor\\x2dboat.mount\n"
        "After=var-lib-motor\\x2dboat.mount\nConditionPathExists=!/run/motor-boat-storage.json\n"
        "[Mount]\nWhat=/var/lib/motor-boat/state/workspaces\nWhere=/Users\nType=none\n"
        "Options=bind\n")
    run("systemctl", "disable", mount.name, "Users.mount")
    run("systemctl", "daemon-reload")
    run("systemctl", "restart", "motor-boat-storage.service")
    with tempfile.NamedTemporaryFile(dir=STATE) as source, tempfile.NamedTemporaryFile(dir="/Users") as target:
        source.write(os.urandom(4096))
        source.flush()
        fcntl.ioctl(target.fileno(), 0x40049409, source.fileno())
        if Path(source.name).read_bytes() != Path(target.name).read_bytes():
            raise ValueError("FICLONE content mismatch")


def rustup_component_name(component: str, host: str) -> str:
    name = component.removesuffix('-' + host)
    return {'clippy-preview': 'clippy', 'rustfmt-preview': 'rustfmt'}.get(name, name)


def freeze_stable(manifest, toolchain, spec, host):
    numbered = spec['rustc'].split()[1] + '-' + host
    source = manifest['rust_toolchains'].get(numbered)
    if (not source or source['rustc'] != spec['rustc']
            or not set(spec['components']) <= set(source['components'])):
        raise ValueError('frozen stable requires a matching numbered release containing its components')
    root = Path(manifest['rustup_home']) / 'toolchains'
    # Rustup reserves official channel names, so `toolchain link stable` fails.
    # An independent copy lets stable omit clippy/rustfmt without removing them
    # from the numbered release or consulting the moving stable channel.
    run('rustup', 'toolchain', 'uninstall', toolchain)
    run('cp', '-a', '--reflink=auto', str(root / numbered) + '/.', str(root / toolchain))


def install_pnpm(manifest):
    """Install pnpm from the release toolchain.json pins, as root's files under
    umask 022, into the npm global layout beneath /usr/local.

    `npm install --global` as root cannot give that: it creates its directories
    with the caller's umask, and it makes a command's target executable with
    chmod, a step no archive extraction repeats. Everything root runs from
    /usr/local must be root's with no group or other write, and pnpm's commands
    are exactly such targets. install-npm-tool.py unpacks the same registry
    archive and writes the same npm bin links instead (see that script)."""
    spec = manifest["tools"]["pnpm"]
    source = spec["source"]
    name, version = source["package"].split("@", 1)
    # The registry serves one version's metadata at <name>/<version>; the
    # name@version spelling is the pin's notation, not a registry path.
    metadata = json.loads(urllib.request.urlopen(f"{source['registry']}{name}/{version}", timeout=60).read())
    algorithm, published = metadata["dist"]["integrity"].split("-", 1)
    if algorithm != "sha512" or published != source["sha512"]:
        raise ValueError("pnpm registry integrity differs from the manifest pin")
    archive = BUILD / "downloads/pnpm.tgz"
    urllib.request.urlretrieve(metadata["dist"]["tarball"], archive)
    if digest(archive, "sha512") != source["sha512"]:
        raise ValueError("pnpm integrity mismatch")
    run("/usr/bin/python3", str(ROOT / "script/runner/install-npm-tool.py"),
        "--prefix", "/usr/local", "--name", name, "--archive", str(archive),
        "--sha512", source["sha512"], "--url", metadata["dist"]["tarball"])


def install_rust(manifest, installer):
    run(str(installer), "-y", "--no-modify-path", "--profile", manifest["rust_profile"], "--default-toolchain", "none")
    # rustup replaces itself with the newest release during `toolchain install`
    # unless told not to, which would drift from the pinned build-server version.
    run("rustup", "set", "auto-self-update", "disable")
    for line in run('rustup', 'toolchain', 'list', capture=True).splitlines():
        if line.strip() == 'no installed toolchains':
            continue
        name = line.split()[0]
        if name not in manifest['rust_toolchains']:
            run('rustup', 'toolchain', 'uninstall', name)
    for toolchain, spec in sorted(manifest["rust_toolchains"].items(), key=lambda row: row[0].startswith('stable-')):
        host = toolchain.split('-', 1)[1]
        if toolchain.startswith('stable-'):
            freeze_stable(manifest, toolchain, spec, host)
        else:
            args = ["rustup", "toolchain", "install", toolchain, "--profile", manifest["rust_profile"]]
            # Installed IDs include the host; --component takes bare names. The
            # minimal profile supplies Cargo/rustc, and --target supplies rust-std.
            components = {rustup_component_name(component, host) for component in spec['components']
                          if not component.startswith('rust-std-')}
            for component in sorted(components - {'cargo', 'rustc', 'rust-std'}):
                args.extend(("--component", component))
            for target in spec["targets"]:
                args.extend(("--target", target))
            run(*args)
        installed = run('rustup', 'component', 'list', '--installed', '--toolchain', toolchain, capture=True)
        for component in set(installed.splitlines()) - set(spec['components']):
            if component.startswith('rust-std-'):
                run('rustup', 'target', 'remove', '--toolchain', toolchain, component.removeprefix('rust-std-'))
            else:
                run('rustup', 'component', 'remove', '--toolchain', toolchain, rustup_component_name(component, host))
    run("rustup", "default", manifest["rust_default"])


def gnu_aliases(manifest):
    for name, destination in manifest["gnu_coreutils"].items():
        target = Path(destination).with_name(name.removeprefix("gnu"))
        if not target.is_file():
            raise ValueError("missing GNU coreutils executable: " + str(target))
        alias = Path(destination)
        if alias.exists() or alias.is_symlink():
            if alias.resolve() != target.resolve():
                raise ValueError("refusing to overwrite coreutils alias: " + str(alias))
        else:
            alias.symlink_to(target)


def tools(manifest, role="boat"):
    pins = {name: spec["version"].removeprefix("v").removeprefix("nats-server: v")
            for name, spec in manifest["tools"].items() if "version" in spec}
    pins["rustup"] = manifest["rustup"]
    pins["nats"] = pins["nats-server"]
    (BUILD / "downloads").mkdir(parents=True, exist_ok=True)
    (BUILD / "tmp").mkdir(exist_ok=True)
    os.environ.update(CARGO_HOME="/srv/build/cargo-home", RUSTUP_HOME="/home/ubuntu/.rustup",
                      TMPDIR="/srv/build/tmp", CARGO_TARGET_DIR="/srv/build/target",
                      RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", GIT_TERMINAL_PROMPT="0",
                      CARGO_BUILD_JOBS=os.environ.get("CARGO_BUILD_JOBS", "4"),
                      PATH="/home/ubuntu/.bun/bin:/srv/build/cargo-home/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
    rustup = (f"https://static.rust-lang.org/rustup/archive/{pins['rustup']}/"
              "x86_64-unknown-linux-gnu/rustup-init")
    path = BUILD / "downloads/rustup-init"
    download(rustup, path, urllib.request.urlopen(rustup + ".sha256", timeout=60).read().decode().split()[0])
    path.chmod(0o755)
    install_rust(manifest, path)
    node = published(f"https://nodejs.org/dist/v{pins['node']}/", f"node-v{pins['node']}-linux-x64.tar.xz")
    extract(node, "/usr/local", "-J", "--strip-components=1")
    install_pnpm(manifest)
    bun = published(f"https://github.com/oven-sh/bun/releases/download/bun-v{pins['bun']}/", "bun-linux-x64.zip")
    run("unzip", "-qo", str(bun), "-d", str(BUILD / "downloads"))
    # The tenant image keeps Bun out of /home, which a tenant VM covers with the
    # tenant's own directory; the runner looks for it at this same per-role path.
    bun = manifest["tools"]["bun"]
    bun_path = Path(bun.get("path_by_role", {}).get(role, bun["path"]))
    bun_path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(BUILD / "downloads/bun-linux-x64/bun", bun_path)
    bun_path.chmod(0o755)
    nats = published(f"https://github.com/nats-io/nats-server/releases/download/v{pins['nats']}/",
                     f"nats-server-v{pins['nats']}-linux-amd64.tar.gz", "SHA256SUMS")
    extract(nats, BUILD / "downloads", "-z")
    run("install", "-m", "0755", str(BUILD / f"downloads/nats-server-v{pins['nats']}-linux-amd64/nats-server"),
        "/usr/local/bin/nats-server")
    for name in ("gcc", "g++"):
        run("update-alternatives", "--set", "x86_64-w64-mingw32-" + name,
            "/usr/bin/x86_64-w64-mingw32-" + name + "-win32")
    gnu_aliases(manifest)
    run('/usr/bin/python3', str(ROOT / 'script/runner/install-binaryen.py'), str(PARITY_PATH))
    # Ubuntu 24.04's git is older than the build server's and behaves
    # differently; build the server's release ahead of it on PATH. The
    # anonymous ls-remote below then also exercises the build's https support.
    run("bash", str(ROOT / "script/runner/build-git.sh"))
    # Fail anonymously instead of forwarding an operator's repository credentials.
    run("git", "-c", "credential.helper=", "ls-remote", "https://github.com/cortexkit/commons", "HEAD")
    run("bash", str(ROOT / "script/runner/install-tools.sh"), role)
    marker = BUILD / "sccache-inputs.sha256"
    artifacts_path = BUILD / 'sccache-artifacts.json'
    inputs = digest(ROOT / "script/runner/build-sccache.sh") + digest(
        ROOT / "script/runner/patches/sccache-0.18.0-read-only-hits.patch")
    artifacts = json.loads(artifacts_path.read_text()) if artifacts_path.is_file() else {}
    binary_hashes = {name: digest(BUILD / 'cargo-home/bin' / name) if (BUILD / 'cargo-home/bin' / name).is_file() else None
                     for name in manifest['sccache']['build_server_sha256']}
    if (not marker.exists() or marker.read_text() != inputs or artifacts.get('inputs_sha256') != inputs
            or artifacts.get('binaries') != binary_hashes or not all(binary_hashes.values())):
        run("sh", str(ROOT / "script/runner/build-sccache.sh"))
        marker.write_text(inputs)
        artifacts_path.write_text(json.dumps({'inputs_sha256': inputs, 'binaries': {
            name: digest(BUILD / 'cargo-home/bin' / name) for name in binary_hashes}}, indent=2) + '\n')


def target_directories():
    # /usr/local can be staff-writable; the helper's trust check includes every parent.
    parents = ('/', '/usr', '/usr/local', '/usr/local/libexec')
    for name in parents:
        path = Path(name)
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise ValueError('permissions helper ancestor is not a physical directory: ' + name)
    run('install', '-d', '-o', 'root', '-g', 'root', '-m', '0755',
        *parents, '/usr/local/bin', '/etc/systemd/system',
        '/etc/ssh/sshd_config.d', '/etc/motor', '/etc/motor/tenant')


def install_permission_helper(binary):
    target_directories()
    run('install', '-o', 'root', '-g', 'root', '-m', '0755', str(binary),
        '/usr/local/libexec/motor-runner-permissions')


def service_preflight():
    try:
        run('/usr/bin/systemctl', 'start', 'motor-boat.service')
        run('/usr/bin/systemctl', 'is-active', '--quiet', 'motor-boat.service')
    except subprocess.CalledProcessError:
        for unit in ('motor-boat-restored.service', 'motor-boat-storage.service', r'var-lib-motor\x2dboat.mount', 'Users.mount', 'motor-boat.service', 'motor-boat-host.service', 'motor-boat-identity.service', 'motor-boat-bridge.service'):
            subprocess.run(['/usr/bin/systemctl', 'status', '--no-pager', '-l', unit], check=False)
            subprocess.run(['/usr/bin/journalctl', '-u', unit, '-b', '--no-pager', '-n', '200'], check=False)
        raise
    finally:
        subprocess.run(['/usr/bin/systemctl', 'stop', 'motor-boat.service'], check=False)


UNITS = ("motor-boat-restored.service", "motor-boat-storage.service", "motor-boat-storage-watch.service", "motor-boat.service", "motor-boat-bridge.service",
         "motor-boat-host.service", "motor-boat-identity.service")
# Units earlier recipes installed; a recipe reapplied on such an image removes them.
RETIRED_UNITS = ("motor-boat-host.path",)


def install_units(systemd=Path("/etc/systemd/system")):
    """Write the unit files and their helper scripts. Runs only while the image
    is built; nothing rewrites units at runtime, so the snapshot holds exactly
    what systemd verified here. The caller reloads systemd afterwards. The units
    carry no CPU count: the image is built on one Boat size and cloned onto
    others, so motor-boat-start.py measures the CPUs on every start."""
    for name in ("motor-boat-network", "motor-boat-bridge.py", "motor-boat-start.py", "motor-boat-credentials.py"):
        run("install", "-m", "0755", str(HERE / name), "/usr/local/libexec/" + name)
    run("install", "-m", "0644", str(ROOT / "script/runner/motor-net.nft"),
        "/usr/local/libexec/motor-boat-net.nft")
    for name in RETIRED_UNITS:
        if (systemd / name).exists() or (systemd / name).is_symlink():
            subprocess.run(["systemctl", "disable", "--now", name], check=False)
            (systemd / name).unlink(missing_ok=True)
    for name in UNITS:
        (systemd / name).write_text((HERE / name).read_text())
    (systemd / "motor-jobs.slice").write_text(
        (ROOT / "script/runner/units/system/motor-jobs.slice").read_text())


def accounts():
    job_account.ensure(json.loads(PARITY_PATH.read_text())['job_identity_by_role']['boat'], run)
    for account in ("motor-cache",):
        if subprocess.run(["id", "-u", account], capture_output=True).returncode:
            run("useradd", "--system", "--user-group", "--home-dir", "/nonexistent",
                "--shell", "/usr/sbin/nologin", account)


def configure(pins, revision):
    target_directories()
    accounts()
    cpus = int(run("nproc", capture=True))
    # The runner-disk mount written by state() requires motor-boat-restored.service.
    install_units()
    state(pins)
    inotify = '/etc/sysctl.d/60-motor-inotify.conf'
    shutil.copy2(ROOT / 'script/runner/sysctl/60-motor-inotify.conf', inotify)
    run('sysctl', '-p', inotify)
    config = Path("/etc/ssh/sshd_config").read_text()
    if not re.search(r"(?im)^\s*Include\s+/etc/ssh/sshd_config.d/\*\.conf\s*$", config):
        raise ValueError("sshd does not include drop-ins; refusing an unverified policy")
    Path("/etc/ssh/sshd_config.d/00-motor-boat.conf").write_text(
        "AllowTcpForwarding local\nPermitTTY no\nAllowAgentForwarding no\nX11Forwarding no\nPermitTunnel no\n")
    run("/usr/sbin/sshd", "-t")
    run("systemctl", "reload", "ssh")
    (STATE / 'host-journal').mkdir(mode=0o700, exist_ok=True)
    Path('/etc/motor/sandbox.json').write_text(json.dumps({
        'journal_dir': str(STATE / 'host-journal'),
        'job_socket': '/run/motor-tenant/jobs.sock',
        'event_socket': '/run/motor-tenant/events.sock',
        'admin_socket': '/run/motor-tenant/admin.sock',
        # The build sandbox's CPU count. motor-boat-start.py --host-exec replaces it with
        # the running sandbox's own count on every endpoint start (/run/motor/sandbox.json).
        'vcpu_ceiling': cpus,
        'ready_marker': '/run/motor/endpoint-ready',
        'storage_health': {
            'state_file': '/run/motor-boat-storage/health.json',
            'lock_file': '/run/motor-boat-storage/lock',
            'identity_paths': ['/var/lib/motor-boat', '/Users'],
        },
        'disk_floor': {'path': str(STATE / 'state'), 'floor_bytes': FREE_SPACE_FLOOR_BYTES},
    }, indent=2) + '\n')
    Path('/etc/motor/image-revision').write_text(revision + '\n')
    Path("/etc/motor/tenant").mkdir(parents=True, exist_ok=True)
    Path("/etc/motor/tenant/id").unlink(missing_ok=True)
    run("systemctl", "daemon-reload")
    run("systemd-analyze", "verify", *("/etc/systemd/system/" + name for name in (*UNITS, "motor-jobs.slice")))
    run("systemctl", "enable", "motor-boat.service", "motor-boat-identity.service")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", help="exact clean source commit to build")
    parser.add_argument('--cargo-seed-manifest', type=Path)
    parser.add_argument('--cargo-seed-archive', type=Path)
    parser.add_argument('--cargo-seed-sha256')
    parser.add_argument('--acknowledge-tenant-one-private-cache', action='store_true',
                        help='seed can contain private fleet Git dependencies; never use for customer images')
    parser.add_argument("--apt-preflight-only", action="store_true", help="record official archive index reachability without changing the guest")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("apply as root")
    pins = json.loads((HERE / "pins.json").read_text())
    manifest = json.loads(PARITY_PATH.read_text())
    validate(pins)
    if args.apt_preflight_only:
        print(json.dumps(archive_preflight(pins)))
        return
    if not args.revision:
        parser.error("installation requires --revision")
    if not (args.cargo_seed_manifest and args.cargo_seed_archive and args.cargo_seed_sha256
            and args.acknowledge_tenant_one_private_cache):
        parser.error('fleet image installation requires an operator-supplied Cargo seed, its manifest SHA-256, and --acknowledge-tenant-one-private-cache')
    seed_manifest = parity.seed.verified_manifest(args.cargo_seed_manifest, args.cargo_seed_sha256)
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
    packages = (manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"]["boat"]
                + manifest["tools"]["git"]["source"]["build_packages"])
    held_packages = apt_install(packages)
    # Boat's base ships Ubuntu's `at`, whose /usr/bin/at is setuid and owned by
    # the daemon account, so the root-owned path check refuses it. The build
    # server has no `at` and nothing here schedules jobs with it, so the image
    # drops the package rather than exempting the path.
    run("apt-get", "purge", *APT_OPTIONS, "-y", *BOAT_BASE_PURGE)
    target_directories()
    accounts()
    # Before the tool installers write under /usr/local, so every entry the
    # provider's sandbox account owns there is Boat's, never ours; the
    # root-owned path check after the service preflight then covers the rest.
    base_layout = boat_base_layout()
    home = tool_home()
    run('/usr/bin/python3', str(HERE / 'motor-boat-credentials.py'), '--output', '/srv/build/boat-provider-audit.json')
    # Check the administrative path and firewall transaction before long tool builds.
    run('/usr/bin/python3', str(HERE / 'motor-boat-network'), '--check', '--template', str(ROOT / 'script/runner/motor-net.nft'))
    tools(manifest)
    seed_inventory = parity.seed.install(args.cargo_seed_manifest, args.cargo_seed_archive,
                                         args.cargo_seed_sha256, manifest['cargo_home'])
    (BUILD / 'cargo-seed.json').write_text(json.dumps(seed_manifest, indent=2) + '\n')
    parity_result = parity.check(manifest, role="boat", cargo_registry=seed_manifest)
    if not parity_result["passed"]:
        raise ValueError("toolchain parity failed: " + json.dumps(parity_result["errors"]))
    os.chdir(ROOT)
    run("cargo", "build", "--release", "--locked", "-p", "motor-runner", "-p", "motor-host")
    for binary in ('motor-runner', 'motor-host'):
        run("install", "-m", "0755", '/srv/build/target/release/' + binary, '/usr/local/bin/' + binary)
    install_permission_helper('/srv/build/target/release/motor-runner')
    run('/usr/local/bin/motor-runner', 'check-permissions-helper')
    configure(pins, args.revision)
    # Starting motor-boat.service checks its configured flags and socket readiness without a job.
    service_preflight()
    # Last, after every installer and the service preflight could have added files.
    owned = root_paths.require(root_paths.BOAT_ROOTS)
    inventory: dict[str, object] = {name: run(*command, capture=True) for name, command in {
        "runner": ["motor-runner", "--version"], "rust": ["rustc", "--version"],
        "node": ["node", "--version"], "bun": ["bun", "--version"],
        "pnpm": ["pnpm", "--version"], "git": ["git", "--version"],
        "wasm-opt": ["wasm-opt", "--version"], "nats": ["nats-server", "--version"], "python": ["python3", "--version"],
        "systemd": ["systemctl", "--version"],
    }.items()}
    inventory.update(revision=args.revision, runner_sha256=digest("/usr/local/bin/motor-runner"),
                      cargo_seed=seed_inventory, tool_home=home, boat_base_layout=base_layout,
                      root_owned_paths=owned,
                       packages=run("dpkg-query", "-W", capture=True), apt_release_files=releases,
                       apt_held_packages=held_packages,
                      parity=parity_result,
                      provider_before_install=json.loads(Path('/srv/build/boat-provider-audit.json').read_text()))
    (STATE / "inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
    print(json.dumps(inventory))


if __name__ == "__main__":
    main()
