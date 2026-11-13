#!/usr/bin/env bash
# Administrator bootstrap/update only; this script is not a delegated sudo command.
# Usage: sudo bash script/runner/install-permissions-helper.sh /absolute/candidate ubuntu --seal-instance STATE=ROOT[,ROOT...]
set -euo pipefail
if [ "$#" -lt 4 ] || [ "$(id -u)" != 0 ]; then
  echo "Run as root: $0 /absolute/path/to/motor-runner trusted-operator --seal-instance STATE=ROOT[,ROOT...]" >&2
  exit 2
fi
binary=$1
operator=$2
shift 2
here=$(cd -- "$(dirname -- "$0")" && pwd)
helper=/usr/local/libexec/motor-runner-permissions
rule=/etc/sudoers.d/motor-runner-permissions
authority=/etc/motor-runner/source-seal.json
python3 - "$binary" <<'PY'
import pathlib, stat, sys
source = pathlib.Path(sys.argv[1])
if not source.is_absolute() or source.is_symlink() or not source.is_file():
    raise SystemExit('candidate must be an absolute regular binary path')
for name in ('/usr','/usr/local','/usr/local/libexec','/etc','/etc/sudoers.d','/etc/motor-runner'):
    path = pathlib.Path(name)
    if not path.exists() and not path.is_symlink():
        path.mkdir(mode=0o755)
    mode = path.lstat()
    if not stat.S_ISDIR(mode.st_mode) or mode.st_uid != 0 or mode.st_mode & 0o022:
        raise SystemExit('helper/rule ancestors must be physical root-owned non-writable directories')
for name in ('/usr/local/libexec/motor-runner-permissions','/etc/sudoers.d/motor-runner-permissions','/etc/motor-runner/source-seal.json'):
    path = pathlib.Path(name)
    if path.exists() or path.is_symlink():
        mode = path.lstat()
        if not stat.S_ISREG(mode.st_mode) or mode.st_uid != 0 or mode.st_mode & 0o022:
            raise SystemExit('refusing an unsafe existing helper or sudoers node')
PY
staged_helper=$(mktemp /usr/local/libexec/.motor-runner-permissions.XXXXXX)
staged_rule=$(mktemp /etc/sudoers.d/.motor-runner-permissions.XXXXXX)
staged_authority=$(mktemp /etc/motor-runner/.source-seal.XXXXXX)
trap 'rm -f -- "$staged_helper" "$staged_rule" "$staged_authority"' EXIT
python3 "$here/source_seal_config.py" "$@" >"$staged_authority"
chown root:root "$staged_authority"
chmod 0644 "$staged_authority"
install -o root -g root -m 0755 -- "$binary" "$staged_helper"
python3 "$here/permissions_policy.py" "$here/permissions-helper.sudoers" "$operator" "$staged_rule"
chmod 0440 "$staged_rule"
visudo -cf "$staged_rule"
"$staged_helper" permission-helper-digest
stamp=$(date -u +%Y%m%dT%H%M%SZ)
if [ -f "$helper" ]; then cp -p -- "$helper" "$helper.rollback-$stamp"; fi
if [ -f "$rule" ]; then cp -p -- "$rule" "$rule.rollback-$stamp"; fi
if [ -f "$authority" ]; then cp -p -- "$authority" "$authority.rollback-$stamp"; fi
mv -f -- "$staged_helper" "$helper"
mv -f -- "$staged_rule" "$rule"
mv -f -- "$staged_authority" "$authority"
visudo -cf "$rule"
echo "Installed root0755 $helper and root0440 $rule for $operator; no job account grant."
echo "Helper grants physical-chmod, seal JOB_ID STATE_ROOT and unseal JOB_ID STATE_ROOT."
echo "It also grants check-staging-links REPO_KEY FILL_UUID STATE_ROOT and remove-staging REPO_KEY FILL_UUID STATE_ROOT only."
echo "Delegated seals and staging operations are restricted by root-owned $authority; pass every instance's owned and caller-host roots. In-process root staging uses the runner's root-controlled launch configuration."
echo "Run the candidate check-permissions-helper as $operator before deploy."
