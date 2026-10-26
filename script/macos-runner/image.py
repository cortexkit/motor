#!/usr/bin/env python3
"""Restore, provision, measure and seal a macOS image using our VM helper."""

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile

from download import fetch
import parity
import pinlock

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parent.parent
REMOTE = "/Users/motorbuild/motor-image-build"
IMAGE_FILES = ("disk.img", "auxiliary-storage", "hardware-model", "machine-identifier")
AGENT = "/opt/motor/bin/motor-macagent"
TCC_SERVICES = ("kTCCServiceSystemPolicyNetworkVolumes", "kTCCServiceSystemPolicyAllFiles")


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


@contextmanager
def mounted_data(disk, *, writable=False):
    attached = plistlib.loads(run("/usr/bin/hdiutil", "attach", "-nomount",
                                 "-readwrite" if writable else "-readonly", "-plist", disk,
                                 capture_output=True).stdout)
    devices = [row["dev-entry"] for row in attached["system-entities"] if "dev-entry" in row]
    device = next((value for value in devices if re.fullmatch(r"/dev/disk[0-9]+", value)), devices[0])
    mount = None
    try:
        apfs = plistlib.loads(run("/usr/sbin/diskutil", "apfs", "list", "-plist", capture_output=True).stdout)
        volumes = [volume["DeviceIdentifier"] for container in apfs["Containers"]
                   if container["PhysicalStores"] and all("/dev/" + store["DeviceIdentifier"] in devices
                                                         for store in container["PhysicalStores"])
                   for volume in container["Volumes"] if "Data" in volume.get("Roles", [])]
        if len(volumes) != 1:
            raise ValueError("sealed image must have exactly one APFS Data volume")
        mount = Path(tempfile.mkdtemp(prefix="motor-image-data-"))
        run("/sbin/mount_apfs", "-o", "nobrowse" if writable else "nobrowse,rdonly", "/dev/" + volumes[0], mount)
        try:
            yield mount
        finally:
            run("/sbin/umount", mount)
    finally:
        try:
            run("/usr/bin/hdiutil", "detach", device)
        finally:
            if mount is not None:
                # Never recursively clean a mount point: a failed detach may leave guest data there.
                mount.rmdir()


def agent_requirement(mount):
    result = run("/usr/bin/codesign", "-d", "-r-", mount / AGENT.lstrip("/"),
                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    requirements = re.findall(r"^#* *designated => (.+)$", result.stdout, re.MULTILINE)
    if len(requirements) != 1 or not requirements[0].strip():
        raise ValueError("installed guest agent has no designated code-signing requirement")
    return requirements[0].strip()


def grant_agent_access(disk):
    # macOS treats virtiofs as a network volume. Without TCC grants, file reads
    # and directory listings fail for the agent and its job children, even as root.
    # SIP protects TCC.db in a running guest, so bind offline grants to the installed signature.
    with mounted_data(disk, writable=True) as mount:
        requirement = agent_requirement(mount)
        database = mount / "Library/Application Support/com.apple.TCC/TCC.db"
        if not database.is_file():
            raise ValueError("sealed image has no system TCC database")
        with tempfile.TemporaryDirectory(prefix="motor-agent-requirement-") as directory:
            source, binary = Path(directory) / "agent.req", Path(directory) / "agent.csreq"
            source.write_text(requirement + "\n")
            run("/usr/bin/csreq", "-r", source, "-b", binary)
            blob = binary.read_bytes()
            if not blob:
                raise ValueError("installed guest agent has an empty compiled requirement")
        sql = "BEGIN IMMEDIATE;\n"
        for service in TCC_SERVICES:
            sql += ("INSERT OR REPLACE INTO access "
                    "(service, client, client_type, auth_value, auth_reason, auth_version, csreq, flags, last_modified) "
                    f"VALUES ('{service}', '{AGENT}', 1, 2, 4, 1, X'{blob.hex()}', 0, "
                    "CAST(strftime('%s','now') AS INTEGER));\n")
        run("/usr/bin/sqlite3", "-bail", database, sql + "COMMIT;")
        run("/bin/sync")
    return {"services": list(TCC_SERVICES), "requirement": requirement}


def verify_ipsw(path, pin):
    if path.stat().st_size != pin["size"] or parity.digest(path) != pin["sha256"]:
        raise ValueError("IPSW build " + pin["build"] + " size/SHA-256 differs")


def require_committed(path):
    relative = path.resolve().relative_to(REPOSITORY)
    run("git", "ls-files", "--error-unmatch", "--", relative, cwd=REPOSITORY, stdout=subprocess.DEVNULL)
    run("git", "diff", "--exit-code", "HEAD", "--", relative, cwd=REPOSITORY)


def ssh_options(args):
    if not re.fullmatch(r"motorbuild@[A-Za-z0-9][A-Za-z0-9.-]*", args.guest):
        raise ValueError("--guest must be motorbuild@<guest-ip-or-host>")
    return ["-F", "/dev/null", "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=yes",
            "-o", "UserKnownHostsFile=" + str(args.known_hosts.resolve()), "-i", str(args.work / "build_key")]


def prepare_inputs(args, manifest):
    inputs = args.work / "inputs"
    inputs.mkdir(exist_ok=True)
    for name, spec in manifest["sources"].items():
        fetch(spec["url"], inputs / name, spec["sha256"], args.measure)
    for tool in manifest["cargo_installs"]:
        if "git" in tool:
            fetch(tool["source"]["url"], inputs / ("cargo-" + tool["rev"]), tool["source"]["sha256"], args.measure)
    commit = manifest["agent"]["commit"]
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise ValueError("agent source needs a full pinned commit")
    with (inputs / "agent.tar").open("wb") as stream:
        run("git", "archive", "--format=tar", commit, cwd=REPOSITORY, stdout=stream)
    (inputs / "agent.commit").write_text(commit + "\n")
    return inputs


def provision(args, manifest):
    if not args.measure:
        pinlock.require_ready(manifest)
        require_committed(args.manifest)
    if args.clt_label:
        if not args.measure and args.clt_label != manifest["clt"]["softwareupdate_label"]:
            raise ValueError("CLT update label differs from reviewed pin")
        manifest["clt"]["softwareupdate_label"] = args.clt_label
    if manifest["clt"]["softwareupdate_label"] == pinlock.PENDING:
        raise ValueError("measure with --clt-label from softwareupdate --list in the guest")
    options = ssh_options(args)
    inputs = prepare_inputs(args, manifest)
    recipe = args.work / "recipe"
    recipe.mkdir(exist_ok=False)
    for name in ("guest.py", "parity.py", "pinlock.py", "bootstrap.sh"):
        shutil.copyfile(HERE / name, recipe / name)
    (recipe / "toolchain.json").write_text(json.dumps(manifest, indent=2) + "\n")
    run("/usr/bin/ssh", *options, args.guest, "test ! -e " + REMOTE + " && mkdir -m 700 " + REMOTE)
    run("/usr/bin/scp", *options, "-r", recipe, inputs, args.guest + ":" + REMOTE + "/")
    # shlex quotes the softwareupdate label, which includes spaces on some releases.
    import shlex
    command = "/bin/sh " + REMOTE + "/recipe/bootstrap.sh " + REMOTE + " " + shlex.quote(manifest["clt"]["softwareupdate_label"]) + (" measure" if args.measure else " reviewed")
    run("/usr/bin/ssh", *options, args.guest, command)
    with (args.work / "candidate.json").open("wb") as stream:
        run("/usr/bin/ssh", *options, args.guest, "sudo -n cat " + REMOTE + "/candidate.json", stdout=stream)
    with (args.work / "tools.tar").open("wb") as stream:
        run("/usr/bin/ssh", *options, args.guest, "sudo -n tar -cf - -C " + REMOTE + " tools", stdout=stream)
    print("Measured candidate: " + str(args.work / "candidate.json"))
    print("NOT DEPLOYABLE: review candidate, mark release_status reviewed, commit lock; then seal this stopped image.")


def seal(args, manifest):
    pinlock.require_ready(manifest)
    parity.validate(manifest, json.loads((HERE.parent / "runner/toolchain.json").read_text()))
    require_committed(args.manifest)
    options = ssh_options(args)
    # Sealing the measured VM avoids rebuilding native binaries into different bytes.
    candidate = json.loads((args.work / "candidate.json").read_text())
    reviewed = dict(manifest, release_status="pending review")
    if candidate != reviewed:
        raise ValueError("reviewed lock does not match this VM's measured candidate")
    run("/usr/bin/scp", *options, args.manifest, args.guest + ":" + REMOTE + "/recipe/toolchain.json")
    result = subprocess.run(["/usr/bin/ssh", *options, args.guest,
                             "sudo -n /usr/bin/python3 " + REMOTE + "/recipe/guest.py seal --scratch " + REMOTE],
                            text=True, capture_output=True)
    marker = "MOTOR_SEALED " + parity.digest(args.manifest)
    if marker not in result.stdout.splitlines() or result.returncode not in (0, 255):
        raise ValueError("guest did not acknowledge Remote Login/key sealing: " + result.stderr)
    (args.work / "guest-sealed.json").write_text(json.dumps({"manifest_sha256": parity.digest(args.manifest), "remote_login": "Off"}) + "\n")
    (args.work / "build_key").unlink()
    (args.work / "build_key.pub").unlink(missing_ok=True)
    print("Guest acknowledged seal and shutdown. Wait for the image-build helper to exit, then run finalize.")


def require_stopped(directory):
    for name in IMAGE_FILES:
        path = directory / name
        if path.is_symlink() or not path.is_file():
            raise ValueError("image must contain four regular files")
        result = subprocess.run(["/usr/sbin/lsof", "-t", "--", str(path)], capture_output=True)
        if result.returncode != 1 or result.stdout or result.stderr:
            raise ValueError("image still open, or lsof could not prove it stopped")


def write_seal(directory, manifest_sha256, ipsw, agent, tcc):
    (directory / "agent.sha256").write_text(agent["sha256"] + "\n")
    files = {name: parity.digest(directory / name) for name in (*IMAGE_FILES, "agent.sha256")}
    (directory / "image.json").write_text(json.dumps({"manifest_sha256": manifest_sha256,
                                                    "ipsw": ipsw, "agent": agent,
                                                    "tcc": tcc, "files": files}, indent=2) + "\n")
    for path in directory.iterdir():
        path.chmod(0o444)
    directory.chmod(0o555)


def finalize(args, manifest):
    pinlock.require_ready(manifest)
    require_committed(args.manifest)
    receipt = json.loads((args.work / "guest-sealed.json").read_text())
    if receipt != {"manifest_sha256": parity.digest(args.manifest), "remote_login": "Off"} or (args.work / "build_key").exists():
        raise ValueError("missing guest seal acknowledgement or build key still exists")
    require_stopped(args.image)
    tcc = grant_agent_access(args.image / "disk.img")
    require_stopped(args.image)
    write_seal(args.image, parity.digest(args.manifest), manifest["ipsw"], manifest["agent"], tcc)
    print("Sealed read-only image; installed-daemon qualification is still required.")


def sealed_source(directory):
    inventory = {*IMAGE_FILES, "agent.sha256"}
    if directory.is_symlink() or not directory.is_dir() or directory.stat().st_mode & 0o222:
        raise ValueError("source must be a read-only sealed image directory")
    if {path.name for path in directory.iterdir()} != inventory | {"image.json"}:
        raise ValueError("source sealed image inventory differs")
    for name in (*inventory, "image.json"):
        path = directory / name
        if path.is_symlink() or not path.is_file() or path.stat().st_mode & 0o222:
            raise ValueError("source sealed image file is not regular and read-only: " + name)
    pin = json.loads((directory / "image.json").read_text())
    if set(pin["files"]) != inventory:
        raise ValueError("source sealed image digest inventory differs")
    for name, sha in pin["files"].items():
        if parity.digest(directory / name) != sha:
            raise ValueError("source sealed image digest differs: " + name)
    if (directory / "agent.sha256").read_text() != pin["agent"]["sha256"] + "\n":
        raise ValueError("source agent digest record differs")
    # Images sealed before offline TCC grants existed carry no record; the swap
    # grants the replacement agent either way. A record that is present must be valid.
    if "tcc" in pin and (pin["tcc"]["services"] != list(TCC_SERVICES)
                         or not pin["tcc"]["requirement"].strip()):
        raise ValueError("source TCC grant record is malformed")
    require_stopped(directory)
    return pin


def guest_path(mount, relative):
    relative = Path(relative)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError("invalid guest path: " + str(relative))
    path = mount
    for part in relative.parts:
        path = path / part
        if path.is_symlink():
            raise ValueError("guest path must not contain symlinks: " + str(relative))
    return path


def require_locked_build_account(mount):
    node = "private/var/db/dslocal/nodes/Default/"
    account = guest_path(mount, node + "users/motorbuild.plist")
    if not account.exists():
        print("Source motorbuild account: absent")
        return
    user = plistlib.loads(account.read_bytes())
    group = guest_path(mount, node + "groups/admin.plist")
    membership = "unknown (admin.plist absent)"
    if group.exists():
        admin = plistlib.loads(group.read_bytes())
        direct = ("motorbuild" in admin.get("users", [])
                  or bool(set(user.get("generateduid", [])) & set(admin.get("groupmembers", []))))
        membership = "present" if direct else "no direct entry"
    print("Source motorbuild direct admin membership: " + membership)

    def refuse(reason):
        raise ValueError("source motorbuild is not provably locked: " + reason)

    if user.get("name") != ["motorbuild"] or user.get("shell") != ["/usr/bin/false"]:
        refuse("name/shell must identify motorbuild with /usr/bin/false")
    if user.get("passwd") != ["*"]:
        refuse("passwd must be the unusable '*' marker")
    authorities = user.get("authentication_authority", [])
    hashes = user.get("ShadowHashData", [])
    if (not isinstance(authorities, list) or not all(isinstance(value, str) for value in authorities)
            or not isinstance(hashes, list) or not all(isinstance(value, bytes) for value in hashes)):
        refuse("malformed authentication_authority or ShadowHashData")
    # The password marker alone does not disable ShadowHash or other authentication methods.
    # Without an authority or stored hash, only the unusable '*' password remains.
    if authorities:
        if not all(value.startswith(";DisabledUser;") for value in authorities):
            refuse("authentication_authority contains an enabled or unknown method")
    elif hashes:
        refuse("ShadowHashData remains without a DisabledUser authority")
    if user.get("home") != ["/Users/motorbuild"]:
        refuse("home must be /Users/motorbuild so SSH keys can be checked")
    ssh = guest_path(mount, "Users/motorbuild/.ssh")
    if ssh.exists() and (not ssh.is_dir() or any(ssh.iterdir())):
        refuse("home .ssh contains key files or other unreviewed SSH configuration")
    print("Source motorbuild account: locked (non-login shell, no enabled password authority, no SSH key files)")


def replace_agent(mount, binary, sha256):
    installed = guest_path(mount, AGENT.lstrip("/"))
    if not installed.is_file():
        raise ValueError("source has no installed guest agent")
    fd, name = tempfile.mkstemp(prefix=".motor-macagent-", dir=installed.parent)
    staged = Path(name)
    os.close(fd)
    try:
        shutil.copyfile(binary, staged)
        os.chown(staged, 0, 0)
        staged.chmod(0o555)
        if parity.digest(staged) != sha256:
            raise ValueError("replacement agent SHA-256 differs from reviewed pin")
        os.replace(staged, installed)
        if parity.digest(installed) != sha256:
            raise ValueError("installed agent SHA-256 differs from reviewed pin")
    finally:
        staged.unlink(missing_ok=True)


def swap_guest(mount, args, manifest, source):
    require_locked_build_account(mount)
    prefix = guest_path(mount, "opt/motor")
    lock = guest_path(mount, "opt/motor/toolchain.json")
    seal_receipt = guest_path(mount, "opt/motor/sealed.json")
    agent_marker = guest_path(mount, "opt/motor/agent.sha256")
    installed = guest_path(mount, AGENT.lstrip("/"))
    old = json.loads(lock.read_text())
    pinlock.require_ready(old)
    if (parity.digest(lock) != source["manifest_sha256"]
            or old["ipsw"] != source["ipsw"] or old["agent"] != source["agent"]):
        raise ValueError("source guest lock differs from sealed metadata")
    if {key: value for key, value in old.items() if key != "agent"} != {key: value for key, value in manifest.items() if key != "agent"}:
        raise ValueError("agent-swap permits only agent changes to the manifest")
    if parity.digest(installed) != source["agent"]["sha256"]:
        raise ValueError("source installed agent digest differs")
    if agent_marker.read_text() != source["agent"]["sha256"] + "\n":
        raise ValueError("source guest agent digest record differs")
    if "tcc" in source and agent_requirement(mount) != source["tcc"]["requirement"]:
        raise ValueError("source installed requirement differs from sealed TCC grant")
    receipt = json.loads(seal_receipt.read_text())
    if receipt != {"manifest_sha256": source["manifest_sha256"], "agent_sha256": source["agent"]["sha256"], "remote_login": "Off"}:
        raise ValueError("source guest seal receipt differs")
    replace_agent(mount, args.agent, manifest["agent"]["sha256"])
    for name, payload in (("toolchain.json", args.manifest.read_bytes()),
                          ("agent.sha256", (manifest["agent"]["sha256"] + "\n").encode()),
                          ("sealed.json", (json.dumps({"manifest_sha256": parity.digest(args.manifest),
                                                       "agent_sha256": manifest["agent"]["sha256"],
                                                       "remote_login": "Off"}) + "\n").encode())):
        path = prefix / name
        path.chmod(0o644)
        path.write_bytes(payload)
        path.chmod(0o444)
    run("/bin/sync")


def agent_swap(args, manifest):
    pinlock.require_ready(manifest)
    require_committed(args.manifest)
    if args.agent.is_symlink() or not args.agent.is_file() or parity.digest(args.agent) != manifest["agent"]["sha256"]:
        raise ValueError("replacement agent SHA-256 differs from reviewed pin")
    source = sealed_source(args.source)
    if args.destination.resolve().is_relative_to(args.source.resolve()):
        raise ValueError("destination must be outside the sealed source directory")
    if args.destination.exists() or args.destination.is_symlink():
        raise ValueError("destination already exists")
    staging = Path(tempfile.mkdtemp(prefix="." + args.destination.name + "-", dir=args.destination.parent))
    try:
        for name in IMAGE_FILES:
            run("/bin/cp", "-c", args.source / name, staging / name)
            if parity.digest(staging / name) != source["files"][name]:
                raise ValueError("cloned image digest differs: " + name)
            (staging / name).chmod(0o600)
        with mounted_data(staging / "disk.img", writable=True) as mount:
            swap_guest(mount, args, manifest, source)
        tcc = grant_agent_access(staging / "disk.img")
        require_stopped(staging)
        write_seal(staging, parity.digest(args.manifest), source["ipsw"], manifest["agent"], tcc)
        if args.destination.exists() or args.destination.is_symlink():
            raise ValueError("destination already exists")
        staging.rename(args.destination)
    finally:
        if staging.exists():
            staging.chmod(0o700)
            shutil.rmtree(staging)
    print("Swapped and sealed read-only image; installed-daemon qualification is still required.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("key", "restore", "boot", "provision", "seal", "finalize", "agent-swap"))
    parser.add_argument("--manifest", type=Path, default=HERE / "toolchain.json")
    parser.add_argument("--work", type=Path)
    parser.add_argument("--from", dest="source", type=Path)
    parser.add_argument("--to", dest="destination", type=Path)
    parser.add_argument("--agent", type=Path)
    parser.add_argument("--image", type=Path)
    parser.add_argument("--ipsw", type=Path)
    parser.add_argument("--vm-helper", type=Path)
    parser.add_argument("--guest")
    parser.add_argument("--known-hosts", type=Path)
    parser.add_argument("--clt-label")
    parser.add_argument("--measure", action="store_true")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("image operations require a dedicated macOS VM host")
    if args.operation != "agent-swap":
        if not args.work:
            parser.error(args.operation + " needs --work")
        args.work = args.work.resolve()
        args.work.mkdir(parents=True, exist_ok=True)
    manifest = json.loads(args.manifest.read_text())
    if args.operation == "agent-swap":
        if not args.source or not args.destination or not args.agent:
            parser.error("agent-swap needs --from --to --agent")
        agent_swap(args, manifest)
    elif args.operation == "key":
        run("/usr/bin/ssh-keygen", "-t", "ed25519", "-N", "", "-f", args.work / "build_key", "-C", "motor-image-build-only")
        print("Authorize only build_key.pub in the disposable guest; never authorize it on the host.")
    elif args.operation in ("restore", "boot"):
        if not args.vm_helper or not args.image or (args.operation == "restore" and not args.ipsw):
            parser.error("restore/boot need --vm-helper --image; restore also needs --ipsw")
        run("/usr/bin/codesign", "--force", "--sign", "-", "--entitlements", HERE / "virtualization.entitlements", args.vm_helper)
        if args.operation == "restore":
            verify_ipsw(args.ipsw, manifest["ipsw"])
            # Loading a restore image talks to the Virtualization installation
            # service, which refuses a process without the virtualization
            # entitlement, so the interpreted `swift script` can't do it.
            prepare = args.work / "prepare-image"
            run("/usr/bin/swiftc", "-O", HERE / "prepare-image.swift", "-o", prepare)
            run("/usr/bin/codesign", "--force", "--sign", "-", "--entitlements", HERE / "virtualization.entitlements", prepare)
            run(prepare, args.ipsw, args.image)
        run(args.vm_helper, "image-build", args.image, *([args.ipsw] if args.operation == "restore" else []))
    elif args.operation == "finalize":
        if not args.image:
            parser.error("finalize needs --image")
        finalize(args, manifest)
    else:
        if not args.guest or not args.known_hosts:
            parser.error("provision/seal need --guest and --known-hosts")
        provision(args, manifest) if args.operation == "provision" else seal(args, manifest)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print("image build refused: " + str(error), file=sys.stderr)
        sys.exit(1)
