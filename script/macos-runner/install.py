#!/usr/bin/env python3
"""Install or inspect the dedicated macOS VM runner (sudo from rentamac)."""

import argparse
import json
import os
from pathlib import Path
import plistlib
import pwd
import re
import shutil
import stat
import subprocess
import sys

import parity
import pinlock

HERE = Path(__file__).resolve().parent
LABEL = "io.cortexkit.motor.macos"
PLIST = Path("/Library/LaunchDaemons/" + LABEL + ".plist")
CONFIG = Path("/Library/Application Support/Motor/macos")
SSHD = Path("/etc/ssh/sshd_config")
SSH_ROOT = Path("/etc/ssh/motor")
BEGIN = "# BEGIN motor macos runner"
END = "# END motor macos runner"
FLOOR = 20 * 1024 ** 3
ENTITLEMENT = "com.apple.security.virtualization"


def run(*args):
    return subprocess.run([str(arg) for arg in args], check=True, capture_output=True, text=True).stdout.strip()


def remove_block(text):
    if BEGIN not in text and END not in text:
        return text
    if text.count(BEGIN) != 1 or text.count(END) != 1:
        raise ValueError("ambiguous managed sshd block; refusing to edit")
    start, end = text.index(BEGIN), text.index(END)
    if start == 0 or text[start - 1] != "\n" or end < start:
        raise ValueError("malformed managed sshd block")
    end += len(END)
    if text[end:end + 1] != "\n":
        raise ValueError("malformed managed sshd block terminator")
    return text[:start - 1] + text[end + 1:]


def ssh_block(ca=SSH_ROOT / "vault-ca.pub", principals=SSH_ROOT / "principals"):
    return (f"\n{BEGIN}\nMatch all\nMatch User motor\n"
            f"    TrustedUserCAKeys {ca}\n    AuthorizedPrincipalsFile {principals}\n"
            "    AuthorizedKeysFile none\n    PubkeyAuthentication yes\nMatch all\n" + END + "\n")


def daemon(runner, state_root, instance, vm_config):
    return {
        "Label": LABEL, "UserName": "motor", "GroupName": "staff",
        "ProgramArguments": [str(runner), "serve", "--instance", instance,
                             "--state-root", str(state_root), "--backend", "macos-vm",
                             "--vm-config", str(vm_config), "--max-weight", "8"],
        "EnvironmentVariables": {"HOME": "/Users/motor", "PATH": "/usr/bin:/bin:/usr/sbin:/sbin"},
        "RunAtLoad": True, "KeepAlive": True, "Umask": 0o077,
        "StandardOutPath": str(state_root / "daemon.stdout"),
        "StandardErrorPath": str(state_root / "daemon.stderr"),
    }


def plist_errors(plist):
    args = plist.get("ProgramArguments", [])
    errors = []
    if plist.get("UserName") != "motor":
        errors.append("LaunchDaemon must run as motor")
    if any(arg.startswith("--development") for arg in args):
        errors.append("LaunchDaemon passes a development flag")
    if args.count("--backend") != 1 or args[args.index("--backend") + 1:args.index("--backend") + 2] != ["macos-vm"]:
        errors.append("LaunchDaemon lacks --backend macos-vm")
    return errors


def entitlement(binary):
    run("/usr/bin/codesign", "--verify", "--strict", binary)
    result = subprocess.run(["/usr/bin/codesign", "--display", "--entitlements", ":-", str(binary)],
                            check=True, capture_output=True)
    data = result.stdout
    if b"<?xml" not in data:
        data = result.stderr
    start = data.find(b"<?xml")
    end = data.find(b"</plist>", start)
    return start >= 0 and end >= 0 and plistlib.loads(data[start:end + len(b"</plist>")]).get(ENTITLEMENT) is True


def internal_volume(path):
    info = plistlib.loads(subprocess.run(["/usr/sbin/diskutil", "info", "-plist", str(path)],
                                        check=True, capture_output=True).stdout)
    return info.get("Internal") is True and info.get("FilesystemType") == "apfs"


def tool_errors(tools, manifest):
    errors = []
    for name, spec in manifest["prep_tools"].items():
        path = tools / spec["path"]
        if not path.is_file() or path.is_symlink() or parity.digest(path) != spec["sha256"]:
            errors.append("pinned tool SHA-256 differs: " + name)
    return errors


def vm_configuration(image, helper, tools, manifest):
    version = manifest["rust_toolchains"][manifest["rust_default"]]["rustc"]
    lines = ["image = " + json.dumps(str(image)), "helper = " + json.dumps(str(helper)),
             "toolchain_version = " + json.dumps(version)]
    for name, spec in manifest["prep_tools"].items():
        lines += ["", "[tools." + name + "]", "path = " + json.dumps(str(tools / spec["path"])),
                  "sha256 = " + json.dumps(spec["sha256"])]
    return "\n".join(lines) + "\n"


def image_errors(image, manifest_sha256):
    errors = []
    image_pin = json.loads((image / "image.json").read_text())
    if image_pin["manifest_sha256"] != manifest_sha256:
        errors.append("image was not sealed with the installed pin lock")
    for name, sha in image_pin["files"].items():
        path = image / name
        if (name not in ("disk.img", "auxiliary-storage", "hardware-model", "machine-identifier", "agent.sha256")
                or path.is_symlink() or parity.digest(path) != sha):
            errors.append("sealed image file differs: " + name)
        elif path.stat().st_mode & 0o222:
            errors.append("sealed image file is writable: " + name)
    if set(image_pin["files"]) != {"disk.img", "auxiliary-storage", "hardware-model", "machine-identifier", "agent.sha256"}:
        errors.append("sealed image inventory is incomplete")
    if image.stat().st_mode & 0o222:
        errors.append("sealed image directory is writable")
    return errors


def ssh_errors(text, effective, ca, principals):
    errors = []
    if ssh_block(ca, principals) not in text:
        errors.append("managed sshd Match User motor block is missing or changed")
    values = dict(line.split(None, 1) for line in effective.splitlines() if " " in line)
    if values.get("trustedusercakeys") != str(ca) or values.get("authorizedprincipalsfile") != str(principals):
        errors.append("sshd does not trust the vault CA/principals for motor")
    if values.get("authorizedkeysfile") != "none":
        errors.append("sshd still accepts unmanaged motor authorized keys")
    return errors


def check_install(state_root, image, instance, manifest, config=CONFIG, plist_path=PLIST,
                  sshd=SSHD, ssh_root=SSH_ROOT):
    errors = []
    for path in (state_root, image):
        if not internal_volume(path):
            errors.append("not on an internal APFS volume: " + str(path))
    if shutil.disk_usage(state_root).free < FLOOR:
        errors.append("free space below 20 GiB")
    installed = json.loads((config / "install.json").read_text())
    if installed["manifest_sha256"] != parity.digest(config / "toolchain.json"):
        errors.append("installed pin lock differs")
    helper = Path(installed["helper"])
    if not entitlement(helper):
        errors.append("VM helper lacks virtualization entitlement")
    plist = plistlib.loads(plist_path.read_bytes())
    errors.extend(plist_errors(plist))
    expected = daemon(Path(installed["runner"]), state_root, instance, config / "vm.toml")
    if plist != expected:
        errors.append("LaunchDaemon differs from installed configuration")
    if (config / "vm.toml").read_text() != vm_configuration(image, helper, state_root / "tools", manifest):
        errors.append("VM configuration differs from installed pins")
    socket = state_root / instance / "runner.sock"
    metadata = socket.lstat()
    if not stat.S_ISSOCK(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_uid != pwd.getpwnam("motor").pw_uid:
        errors.append("runner socket must be motor-owned mode 0600")
    errors.extend(tool_errors(state_root / "tools", manifest))
    ca, principals = ssh_root / "vault-ca.pub", ssh_root / "principals"
    if parity.digest(ca) != installed["vault_ca_sha256"] or parity.digest(principals) != installed["principals_sha256"]:
        errors.append("vault CA or authorized principals changed")
    effective = run("/usr/sbin/sshd", "-T", "-f", sshd, "-C", "user=motor,host=localhost,addr=127.0.0.1")
    errors.extend(ssh_errors(sshd.read_text(), effective, ca, principals))
    errors.extend(image_errors(image, parity.digest(config / "toolchain.json")))
    return errors


def create_account():
    try:
        account = pwd.getpwnam("motor")
    except KeyError:
        used = {entry.pw_uid for entry in pwd.getpwall()}
        uid = next(value for value in range(501, 1000) if value not in used)
        run("/usr/sbin/dscl", ".", "-create", "/Users/motor")
        for key, value in {"UniqueID": str(uid), "PrimaryGroupID": "20", "NFSHomeDirectory": "/Users/motor",
                           "UserShell": "/bin/sh", "RealName": "Motor VM runner", "Password": "*"}.items():
            run("/usr/sbin/dscl", ".", "-create", "/Users/motor", key, value)
        account = pwd.getpwnam("motor")
    groups = run("/usr/bin/id", "-Gn", "motor").split()
    if account.pw_uid == 0 or "admin" in groups:
        raise ValueError("motor must be a dedicated non-admin account")
    Path("/Users/motor").mkdir(exist_ok=True)
    os.chown("/Users/motor", account.pw_uid, account.pw_gid)
    return account


def install(args, manifest):
    pinlock.require_ready(manifest)
    server = json.loads((HERE.parent / "runner/toolchain.json").read_text())
    parity.validate(manifest, server)
    for path in (args.state_root, args.image):
        ancestor = path
        while not ancestor.exists():
            ancestor = ancestor.parent
        if not internal_volume(ancestor):
            raise ValueError("runner state/image require an internal APFS volume")
    if shutil.disk_usage(args.image).free < FLOOR:
        raise ValueError("free space below 20 GiB")
    if tool_errors(args.tool_bundle, manifest):
        raise ValueError("tool bundle does not match reviewed pins")
    errors = image_errors(args.image, parity.digest(args.manifest))
    if errors:
        raise ValueError("; ".join(errors))
    if not args.vault_ca.read_text().startswith("ssh-ed25519 "):
        raise ValueError("vault CA must be an OpenSSH ed25519 public key")
    account = create_account()
    args.state_root.mkdir(parents=True, exist_ok=True)
    os.chown(args.state_root, account.pw_uid, account.pw_gid)
    args.state_root.chmod(0o700)
    CONFIG.mkdir(parents=True, exist_ok=True)
    CONFIG.chmod(0o755)
    subprocess.run(["/bin/launchctl", "bootout", "system/" + LABEL], capture_output=True)
    for name, source in (("motor-runner", args.runner), ("motor-macvm", args.vm_helper)):
        destination = CONFIG / name
        if source.resolve() != destination.resolve():
            shutil.copyfile(source, destination)
        destination.chmod(0o755)
    # Re-sign the actual installed VM creator on updates as well as first install.
    run("/usr/bin/codesign", "--force", "--sign", "-", "--entitlements", HERE / "virtualization.entitlements", CONFIG / "motor-macvm")
    if not entitlement(CONFIG / "motor-macvm"):
        raise ValueError("signed helper lacks virtualization entitlement")
    tools = args.state_root / "tools"
    if tools.exists():
        shutil.rmtree(tools)
    shutil.copytree(args.tool_bundle, tools, symlinks=False)
    errors = tool_errors(tools, manifest)
    if errors:
        raise ValueError("; ".join(errors))
    for path in [tools, *tools.rglob("*")]:
        os.chown(path, 0, 0)
        path.chmod(0o555 if path.is_dir() or os.access(path, os.X_OK) else 0o444)
    shutil.copyfile(args.manifest, CONFIG / "toolchain.json")
    (CONFIG / "vm.toml").write_text(vm_configuration(args.image, CONFIG / "motor-macvm", tools, manifest))
    SSH_ROOT.mkdir(mode=0o755, exist_ok=True)
    shutil.copyfile(args.vault_ca, SSH_ROOT / "vault-ca.pub")
    (SSH_ROOT / "principals").write_text("motor\n")
    for path in (SSH_ROOT / "vault-ca.pub", SSH_ROOT / "principals"):
        path.chmod(0o644)
    original = SSHD.read_text()
    SSHD.write_text(remove_block(original) + ssh_block())
    try:
        run("/usr/sbin/sshd", "-t", "-f", SSHD)
    except Exception:
        SSHD.write_text(original)
        raise
    installed = {"runner": str(CONFIG / "motor-runner"), "helper": str(CONFIG / "motor-macvm"),
                 "manifest_sha256": parity.digest(CONFIG / "toolchain.json"),
                 "vault_ca_sha256": parity.digest(SSH_ROOT / "vault-ca.pub"),
                 "principals_sha256": parity.digest(SSH_ROOT / "principals")}
    (CONFIG / "install.json").write_text(json.dumps(installed, indent=2) + "\n")
    PLIST.write_bytes(plistlib.dumps(daemon(CONFIG / "motor-runner", args.state_root, args.instance, CONFIG / "vm.toml")))
    PLIST.chmod(0o644)
    run("/bin/launchctl", "bootstrap", "system", PLIST)
    print("Installed VM runner. Run --check after launchd creates the socket; no VM qualification is implied.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", nargs="?", choices=("install", "uninstall"), default="install")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--manifest", type=Path, default=HERE / "toolchain.json")
    parser.add_argument("--state-root", type=Path, default=Path("/Users/motor/.motor"))
    parser.add_argument("--image", type=Path)
    parser.add_argument("--instance", default="macos")
    parser.add_argument("--runner", type=Path)
    parser.add_argument("--vm-helper", type=Path)
    parser.add_argument("--tool-bundle", type=Path)
    parser.add_argument("--vault-ca", type=Path)
    args = parser.parse_args()
    if sys.platform != "darwin" or os.geteuid() != 0:
        parser.error("run on the VM host with sudo from rentamac")
    if args.operation == "uninstall":
        if args.check:
            parser.error("--check and uninstall are exclusive")
        SSHD.write_text(remove_block(SSHD.read_text()))
        run("/usr/sbin/sshd", "-t", "-f", SSHD)
        print("Removed only the managed motor sshd block; accounts, image and service are retained.")
        return 0
    if not args.image or not args.state_root.is_absolute() or not args.image.is_absolute() or not re.fullmatch(r"[A-Za-z0-9_-]+", args.instance):
        parser.error("absolute --image/--state-root and a plain --instance are required")
    manifest_path = CONFIG / "toolchain.json" if args.check else args.manifest
    manifest = json.loads(manifest_path.read_text())
    pinlock.require_ready(manifest)
    parity.validate(manifest, json.loads((HERE.parent / "runner/toolchain.json").read_text()))
    if args.check:
        errors = check_install(args.state_root, args.image, args.instance, manifest)
        print(json.dumps({"passed": not errors, "checks": 9, "errors": errors}, indent=2))
        return int(bool(errors))
    if os.environ.get("SUDO_USER") != "rentamac":
        parser.error("first install and updates must be invoked by rentamac using sudo")
    for name in ("runner", "vm_helper", "tool_bundle", "vault_ca"):
        if getattr(args, name) is None:
            parser.error("install requires --" + name.replace("_", "-"))
    install(args, manifest)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print("macOS runner refused: " + str(error), file=sys.stderr)
        sys.exit(1)
