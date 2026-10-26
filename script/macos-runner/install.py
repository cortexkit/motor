#!/usr/bin/env python3
"""Install or inspect the dedicated macOS VM runner (sudo from rentamac)."""

import argparse
import json
import os
from pathlib import Path
import plistlib
import pwd
import re
import secrets
import shutil
import stat
import subprocess
import sys
import tempfile

import image as image_recipe
import parity
import pinlock

HERE = Path(__file__).resolve().parent
LABEL = "io.cortexkit.motor.macos"
HOME = Path("/Users/motor")
PLIST = HOME / "Library/LaunchAgents" / (LABEL + ".plist")
LEGACY_PLIST = Path("/Library/LaunchDaemons/" + LABEL + ".plist")
PASSWORD = Path("/var/root/motor-autologin-password")
BACKUP = Path("/var/root/motor-autologin-backup.plist")
KCPASSWORD = Path("/etc/kcpassword")
LOGINWINDOW = "/Library/Preferences/com.apple.loginwindow"
CONFIG = Path("/Library/Application Support/Motor/macos")
SSHD = Path("/etc/ssh/sshd_config")
SSH_ROOT = Path("/etc/ssh/motor")
BEGIN = "# BEGIN motor macos runner"
END = "# END motor macos runner"
FLOOR = 20 * 1024 ** 3
ENTITLEMENT = "com.apple.security.virtualization"
SSH_ACCESS_GROUP = "com.apple.access_ssh"


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


# ck-motor signs with one plain Ed25519 key held in the credential vault (no
# CA, no certificates), so sshd pins exactly that public key for `motor`, in a
# root-owned file the account cannot edit.
def ssh_block(keys=SSH_ROOT / "authorized_keys"):
    return (f"\n{BEGIN}\nMatch all\nMatch User motor\n"
            f"    AuthorizedKeysFile {keys}\n    PubkeyAuthentication yes\nMatch all\n" + END + "\n")


def agent(runner, state_root, instance, vm_config):
    return {
        "Label": LABEL, "LimitLoadToSessionType": "Aqua",
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
    if "UserName" in plist or "GroupName" in plist or plist.get("LimitLoadToSessionType") != "Aqua":
        errors.append("LaunchAgent must use the motor Aqua session without UserName/GroupName")
    if any(arg.startswith("--development") for arg in args):
        errors.append("LaunchAgent passes a development flag")
    if args.count("--backend") != 1 or args[args.index("--backend") + 1:args.index("--backend") + 2] != ["macos-vm"]:
        errors.append("LaunchAgent lacks --backend macos-vm")
    return errors


def private_write(path, data):
    # Keep recovery data intact on interrupted updates, and protect secrets before writing.
    if path.is_symlink():
        raise OSError("refusing to replace a symlink: " + str(path))
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            os.fchown(stream.fileno(), 0, 0)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def kcpassword(password):
    key = bytes.fromhex("7D 89 52 23 D2 BC DD EA A3 B9 1F")
    data = password.encode("utf-8") + b"\0"
    data += b"\0" * (-len(data) % 12)
    return bytes(value ^ key[index % len(key)] for index, value in enumerate(data))


def auto_login_user():
    # Exporting the domain distinguishes an absent key from a failed defaults read.
    try:
        values = plistlib.loads(run("/usr/bin/defaults", "export", LOGINWINDOW, "-").encode())
    except subprocess.CalledProcessError:
        if not Path(LOGINWINDOW + ".plist").exists():
            return None
        raise
    return values.get("autoLoginUser")


def require_filevault_off():
    if run("/usr/bin/fdesetup", "status") != "FileVault is Off.":
        raise ValueError("FileVault must be off for motor auto-login; disable it before installing or checking")


def as_motor(*args):
    return ("/usr/bin/sudo", "-H", "-u", "motor", *args)


def secret_run(*args):
    # CalledProcessError includes argv, so never propagate it for password commands.
    result = subprocess.run([str(arg) for arg in args], capture_output=True)
    if result.returncode:
        raise ValueError("runner password/keychain setup failed (exit " + str(result.returncode) + ")")


def login_managed():
    record = CONFIG / "install.json"
    return record.exists() and json.loads(record.read_text()).get("auto_login_managed", False)


def configure_login(account):
    previous = auto_login_user()
    keychain = HOME / "Library/Keychains/login.keychain-db"
    if not BACKUP.exists():
        if login_managed():
            raise ValueError("auto-login backup is missing; restore it before updating")
        backup: dict[str, object] = {"keychain_ready": False}
        if previous is not None:
            backup["autoLoginUser"] = previous
        if KCPASSWORD.exists():
            backup["kcpassword"] = KCPASSWORD.read_bytes()
        if keychain.exists():
            backup["keychain"] = keychain.read_bytes()
        try:
            backup["default_keychain"] = run(*as_motor("/usr/bin/security", "default-keychain", "-d", "user")).strip('"')
        except subprocess.CalledProcessError:
            pass
        private_write(BACKUP, plistlib.dumps(backup))
    backup = plistlib.loads(BACKUP.read_bytes())
    if not PASSWORD.exists():
        if backup["keychain_ready"]:
            raise ValueError("saved motor auto-login password is missing; restore it before updating")
        private_write(PASSWORD, secrets.token_urlsafe(32).encode() + b"\n")
    password = PASSWORD.read_text().strip()
    if not password:
        raise ValueError("saved motor auto-login password is empty")
    private_write(PASSWORD, password.encode() + b"\n")
    secret_run("/usr/bin/dscl", ".", "-passwd", "/Users/motor", password)
    if not backup["keychain_ready"]:
        keychain.parent.mkdir(parents=True, exist_ok=True)
        os.chown(keychain.parent, account.pw_uid, account.pw_gid)
        keychain.unlink(missing_ok=True)
        secret_run(*as_motor("/usr/bin/security", "create-keychain", "-p", password, keychain))
        run(*as_motor("/usr/bin/security", "default-keychain", "-d", "user", "-s", keychain))
        backup["keychain_ready"] = True
        private_write(BACKUP, plistlib.dumps(backup))
    # Virtualization signs with a Secure Enclave key at VM start, even after idle periods.
    secret_run(*as_motor("/usr/bin/security", "unlock-keychain", "-p", password, keychain))
    run(*as_motor("/usr/bin/security", "set-keychain-settings", keychain))
    private_write(KCPASSWORD, kcpassword(password))
    run("/usr/bin/defaults", "write", LOGINWINDOW, "autoLoginUser", "-string", "motor")
    return previous != "motor"


def gui_domain(account):
    return "gui/" + str(account.pw_uid)


def has_gui_session(account):
    try:
        run("/bin/launchctl", "print", gui_domain(account))
        return True
    except subprocess.CalledProcessError:
        return False


def stop_jobs(account):
    for target in ("system/" + LABEL, gui_domain(account) + "/" + LABEL):
        result = subprocess.run(["/bin/launchctl", "bootout", target], capture_output=True)
        if result.returncode:
            try:
                run("/bin/launchctl", "print", target)
            except subprocess.CalledProcessError:
                continue
            raise ValueError("could not stop launchd job: " + target)
    LEGACY_PLIST.unlink(missing_ok=True)


def restore_login():
    if not BACKUP.exists():
        if PASSWORD.exists() or login_managed():
            raise ValueError("auto-login backup is missing; restore it before uninstalling")
        return
    backup = plistlib.loads(BACKUP.read_bytes())
    if auto_login_user() is not None:
        run("/usr/bin/defaults", "delete", LOGINWINDOW, "autoLoginUser")
    prior_motor = backup.get("autoLoginUser") == "motor"
    if "kcpassword" in backup and not prior_motor:
        private_write(KCPASSWORD, backup["kcpassword"])
    else:
        KCPASSWORD.unlink(missing_ok=True)
    if "autoLoginUser" in backup and not prior_motor:
        run("/usr/bin/defaults", "write", LOGINWINDOW, "autoLoginUser", "-string", backup["autoLoginUser"])
    account = pwd.getpwnam("motor")
    keychain = HOME / "Library/Keychains/login.keychain-db"
    keychain.unlink(missing_ok=True)
    if "keychain" in backup:
        private_write(keychain, backup["keychain"])
        os.chown(keychain, account.pw_uid, account.pw_gid)
    if backup.get("default_keychain"):
        run(*as_motor("/usr/bin/security", "default-keychain", "-d", "user", "-s", backup["default_keychain"]))
    else:
        run(*as_motor("/usr/bin/security", "default-keychain", "-d", "user", "-s"))
    # Revoke the installed credential without setting a guessable password or disabling the account.
    secret_run("/usr/bin/dscl", ".", "-passwd", "/Users/motor", secrets.token_urlsafe(32))
    if login_managed():
        record = CONFIG / "install.json"
        installed = json.loads(record.read_text())
        installed["auto_login_managed"] = False
        record.write_text(json.dumps(installed, indent=2) + "\n")
    PASSWORD.unlink(missing_ok=True)
    BACKUP.unlink()
    if prior_motor:
        print("Auto-login is now off because the previous auto-login user was motor. "
              "The operator can re-enable auto-login for their own account.")


def uninstall():
    try:
        stop_jobs(pwd.getpwnam("motor"))
    except KeyError:
        subprocess.run(["/bin/launchctl", "bootout", "system/" + LABEL], capture_output=True)
        LEGACY_PLIST.unlink(missing_ok=True)
    PLIST.unlink(missing_ok=True)
    restore_login()
    original = SSHD.read_text()
    SSHD.write_text(remove_block(original))
    try:
        run("/usr/sbin/sshd", "-t", "-f", SSHD)
    except Exception:
        SSHD.write_text(original)
        raise
    print("Removed runner jobs and managed SSH block; restored login settings/keychain and revoked the saved motor password. "
          "Account, image and runner state are retained. Restart to leave the auto-login session.")


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
    # diskutil accepts a device or a mount point, not an arbitrary path such as
    # /Users (a firmlink into the data volume), so ask stat for the device the
    # path lives on first.
    device = subprocess.run(["/usr/bin/stat", "-f", "%Sd", str(path)],
                            check=True, capture_output=True, text=True).stdout.strip()
    info = plistlib.loads(subprocess.run(["/usr/sbin/diskutil", "info", "-plist", device],
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
    grant = image_pin.get("tcc", {})
    if (not isinstance(grant, dict) or grant.get("services") != list(image_recipe.TCC_SERVICES)
            or not isinstance(grant.get("requirement"), str) or not grant["requirement"].strip()):
        errors.append("sealed image is missing the guest agent TCC grant record")
    elif not errors:
        with image_recipe.mounted_data(image / "disk.img") as mount:
            if image_recipe.agent_requirement(mount) != grant["requirement"]:
                errors.append("guest agent code-signing requirement differs from sealed TCC grant")
    return errors


def ssh_errors(text, effective, keys):
    errors = []
    if ssh_block(keys) not in text:
        errors.append("managed sshd Match User motor block is missing or changed")
    values = dict(line.split(None, 1) for line in effective.splitlines() if " " in line)
    if values.get("authorizedkeysfile") != str(keys):
        errors.append("sshd does not pin the runner key file for motor")
    if values.get("trustedusercakeys", "none") != "none":
        errors.append("sshd trusts a user CA for motor")
    return errors


def keychain_errors():
    keychain = HOME / "Library/Keychains/login.keychain-db"
    result = subprocess.run([str(arg) for arg in as_motor("/usr/bin/security", "show-keychain-info", keychain)],
                            capture_output=True, text=True)
    if result.returncode or "no-timeout" not in result.stdout + result.stderr:
        return ["runner login keychain must have no-timeout; rerun install"]
    return []


def check_install(state_root, image, instance, manifest, config=CONFIG, plist_path=PLIST,
                  sshd=SSHD, ssh_root=SSH_ROOT):
    errors = []
    account = pwd.getpwnam("motor")
    try:
        require_filevault_off()
    except ValueError as error:
        errors.append(str(error))
    if auto_login_user() != "motor":
        errors.append("autoLoginUser must be motor; rerun install and restart")
    errors.extend(keychain_errors())
    if not has_gui_session(account):
        errors.append("motor has no Aqua login session (" + gui_domain(account) + "); restart for auto-login before --check")
    else:
        try:
            job = run("/bin/launchctl", "print", gui_domain(account) + "/" + LABEL)
            if not re.search(r"^\s*state = running\s*$", job, re.MULTILINE):
                errors.append("LaunchAgent is not running in " + gui_domain(account))
        except subprocess.CalledProcessError:
            errors.append("LaunchAgent is not loaded in " + gui_domain(account))
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
    expected = agent(Path(installed["runner"]), state_root, instance, config / "vm.toml")
    if plist != expected:
        errors.append("LaunchAgent differs from installed configuration")
    if LEGACY_PLIST.exists():
        errors.append("legacy LaunchDaemon plist must be removed; rerun install")
    if (config / "vm.toml").read_text() != vm_configuration(image, helper, state_root / "tools", manifest):
        errors.append("VM configuration differs from installed pins")
    socket = state_root / instance / "runner.sock"
    if not socket.exists():
        errors.append("runner socket is missing; wait for the Aqua LaunchAgent after restarting")
    else:
        metadata = socket.lstat()
        if not stat.S_ISSOCK(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_uid != account.pw_uid:
            errors.append("runner socket must be motor-owned mode 0600")
    errors.extend(tool_errors(state_root / "tools", manifest))
    keys = ssh_root / "authorized_keys"
    if parity.digest(keys) != installed["authorized_keys_sha256"]:
        errors.append("pinned runner key changed")
    effective = run("/usr/sbin/sshd", "-T", "-f", sshd, "-C", "user=motor,host=localhost,addr=127.0.0.1")
    errors.extend(ssh_errors(sshd.read_text(), effective, keys))
    errors.extend(image_errors(image, parity.digest(config / "toolchain.json")))
    return errors


def create_account():
    try:
        account = pwd.getpwnam("motor")
    except KeyError:
        used = {entry.pw_uid for entry in pwd.getpwall()}
        uid = next(value for value in range(501, 1000) if value not in used)
        run("/usr/bin/dscl", ".", "-create", "/Users/motor")
        for key, value in {"UniqueID": str(uid), "PrimaryGroupID": "20", "NFSHomeDirectory": "/Users/motor",
                           "UserShell": "/bin/sh", "RealName": "Motor VM runner", "Password": "*"}.items():
            run("/usr/bin/dscl", ".", "-create", "/Users/motor", key, value)
        account = pwd.getpwnam("motor")
    groups = run("/usr/bin/id", "-Gn", "motor").split()
    if account.pw_uid == 0 or "admin" in groups:
        raise ValueError("motor must be a dedicated non-admin account")
    Path("/Users/motor").mkdir(exist_ok=True)
    os.chown("/Users/motor", account.pw_uid, account.pw_gid)
    # When Remote Login is limited to listed users, PAM refuses any account
    # outside com.apple.access_ssh before key authentication is considered.
    run("/usr/sbin/dseditgroup", "-o", "edit", "-a", "motor", "-t", "user", SSH_ACCESS_GROUP)
    return account


def install(args, manifest):
    pinlock.require_ready(manifest)
    require_filevault_off()
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
    runner_key = args.runner_key.read_text()
    if len(runner_key.splitlines()) != 1 or len(runner_key.split()) != 2 or not runner_key.startswith("ssh-ed25519 "):
        raise ValueError("runner key must be one bare OpenSSH ed25519 public key line")
    account = create_account()
    args.state_root.mkdir(parents=True, exist_ok=True)
    os.chown(args.state_root, account.pw_uid, account.pw_gid)
    args.state_root.chmod(0o700)
    CONFIG.mkdir(parents=True, exist_ok=True)
    CONFIG.chmod(0o755)
    stop_jobs(account)
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
    keys = SSH_ROOT / "authorized_keys"
    keys.write_text(runner_key.strip() + "\n")
    os.chown(keys, 0, 0)
    keys.chmod(0o644)
    original = SSHD.read_text()
    SSHD.write_text(remove_block(original) + ssh_block())
    try:
        run("/usr/sbin/sshd", "-t", "-f", SSHD)
    except Exception:
        SSHD.write_text(original)
        raise
    installed: dict[str, object] = {"runner": str(CONFIG / "motor-runner"), "helper": str(CONFIG / "motor-macvm"),
                 "manifest_sha256": parity.digest(CONFIG / "toolchain.json"),
                 "authorized_keys_sha256": parity.digest(SSH_ROOT / "authorized_keys")}
    for directory in (HOME / "Library", PLIST.parent):
        directory.mkdir(parents=True, exist_ok=True)
        os.chown(directory, account.pw_uid, account.pw_gid)
        directory.chmod(0o755)
    login_changed = configure_login(account)
    if login_changed:
        print("Restart required: the auto-login user changed to motor. The installer will not restart the host.")
    private_write(PLIST, plistlib.dumps(agent(CONFIG / "motor-runner", args.state_root, args.instance, CONFIG / "vm.toml")))
    os.chown(PLIST, 0, 0)
    PLIST.chmod(0o644)
    installed["auto_login_managed"] = True
    (CONFIG / "install.json").write_text(json.dumps(installed, indent=2) + "\n")
    if has_gui_session(account):
        run("/bin/launchctl", "bootstrap", gui_domain(account), PLIST)
    else:
        print("motor has no Aqua login session; restart for auto-login to start the LaunchAgent.")
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
    parser.add_argument("--runner-key", type=Path,
                        help="ck-motor's vault SSH public key line (its agent's identity.pub)")
    args = parser.parse_args()
    if sys.platform != "darwin" or os.geteuid() != 0:
        parser.error("run on the VM host with sudo from rentamac")
    if args.operation == "uninstall":
        if args.check:
            parser.error("--check and uninstall are exclusive")
        uninstall()
        return 0
    if not args.image or not args.state_root.is_absolute() or not args.image.is_absolute() or not re.fullmatch(r"[A-Za-z0-9_-]+", args.instance):
        parser.error("absolute --image/--state-root and a plain --instance are required")
    manifest_path = CONFIG / "toolchain.json" if args.check else args.manifest
    manifest = json.loads(manifest_path.read_text())
    pinlock.require_ready(manifest)
    parity.validate(manifest, json.loads((HERE.parent / "runner/toolchain.json").read_text()))
    if args.check:
        errors = check_install(args.state_root, args.image, args.instance, manifest)
        print(json.dumps({"passed": not errors, "checks": 15, "errors": errors}, indent=2))
        return int(bool(errors))
    if os.environ.get("SUDO_USER") != "rentamac":
        parser.error("first install and updates must be invoked by rentamac using sudo")
    for name in ("runner", "vm_helper", "tool_bundle", "runner_key"):
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
