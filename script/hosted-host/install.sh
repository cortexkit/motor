#!/usr/bin/env bash
# Provision this Linux build server as a hosted-runners host: the motor-vm-helper
# root service, the unprivileged motor-host TLS agent, their configs and keys,
# and the agent's one firewall exception. Run as root on the server, from a
# checkout of this repository. docs/hosted-host.md has the full procedure.
#
#   install.sh --checkout DIR --host-id ID --tenant-sub SUB [--build]
#              [--target-dir DIR] [--port N] [--public-ip IPV4]
#              [--tenant-disk-quota-gib N]
#   install.sh --checkout DIR --check [--public-ip IPV4]
#   install.sh --checkout DIR --start [--public-ip IPV4]
#   install.sh --checkout DIR --stop
#
# Installing is idempotent and never enables or starts a unit. A config or unit
# that differs from what this script would write is shown as a diff and
# replaced only after a y answer (end of input is no); the old file is kept as
# <file>.bak-<UTC time>. Keys are created only when none exist and are never
# replaced. The host id and tenant subject are arguments, not repository
# defaults; --check, --start and --stop read them from the installed configs.
# The tenant-1.json template sizes the tenant only on a first install: once
# the tenant record is installed it keeps its sizing and image, which
# provision.py resize and switch-image change, and the helper config takes its
# slot CPUs from that record (--tenant-disk-quota-gib is then refused).
# --check verifies every file and unit this script installs and changes
# nothing. It also checks the installed tenant record itself (provision.py's
# checks, a cpuset clear of the Windows CPUs), that the helper registry entry
# agrees with it, that the helper config is its render, capacity.json, the
# memory fit and the jobs slice cap, and that no resize.json is pending.
# --start runs the same checks,
# then opens the agent's firewall port and enables and starts both units: the
# broker places work on this host from its first signed heartbeat, so this is
# the only step that puts it into service, and an enabled unit would otherwise
# start on the next reboot. --stop is the rollback: it disables and stops both
# units and closes the port, keeping keys, journal, configs and tenant disk.
set -euo pipefail

HOST_USER=motor-host
HOST_BINARY=/usr/local/bin/motor-host
HELPER_BINARY=/usr/local/libexec/motor-vm-helper
LOADER=/usr/local/libexec/motor-vm-loader
FIRECRACKER_DIR=/usr/local/lib/firecracker/1.17.0
HOST_CONFIG=/etc/motor-host/host.json
HELPER_CONFIG=/etc/motor-vm-helper.json
TENANT_CONFIG=/etc/motor-host/provision/tenant-1.json
TENANT_REGISTRY=/etc/motor-vm-helper/tenants/tenant-1.json
BINDINGS=/etc/motor-host/tenants.json
KEY_DIR=/etc/motor-host/keys
SIGNING_KEY=$KEY_DIR/host-signing.key
TLS_CERT=$KEY_DIR/tls.crt
TLS_KEY=$KEY_DIR/tls.key
JOURNAL_DIR=/var/lib/motor-host
STORAGE=/var/lib/motor-vm
HELPER_SOCKET=/run/motor-vm-helper.sock
UNIT_DIR=/etc/systemd/system
LOADER_TARGET=x86_64-unknown-linux-musl

usage() {
  sed -n '2,34p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

checkout='' target_dir=/srv/build/target build=0 mode=install
port='' host_id='' public_ip='' tenant_sub='' tenant_quota='' tenant_image=''
while [ $# -gt 0 ]; do
  case "$1" in
    --checkout) checkout=${2:?}; shift 2 ;;
    --target-dir) target_dir=${2:?}; shift 2 ;;
    --build) build=1; shift ;;
    --check|--start|--stop)
      [ "$mode" = install ] || { echo "--check, --start and --stop are exclusive" >&2; exit 2; }
      mode=${1#--}; shift ;;
    --port) port=${2:?}; shift 2 ;;
    --host-id) host_id=${2:?}; shift 2 ;;
    --public-ip) public_ip=${2:?}; shift 2 ;;
    --tenant-sub) tenant_sub=${2:?}; shift 2 ;;
    --tenant-disk-quota-gib) tenant_quota=${2:?}; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$checkout" ] || usage
if [ "$mode" = install ]; then
  [ -n "$host_id" ] && [ -n "$tenant_sub" ] || { echo "installing needs --host-id and --tenant-sub" >&2; exit 2; }
elif [ "$build" = 1 ] || [ -n "$host_id$tenant_sub$tenant_quota$port" ]; then
  echo "--$mode uses the installed configs; --build, --host-id, --tenant-sub, --tenant-disk-quota-gib and --port are install options" >&2
  exit 2
fi
# Changes are made only when installing; --start changes nothing until every
# check has passed.
check=1
[ "$mode" = install ] && check=0
[ "$(uname -s)" = Linux ] || { echo "hosted hosts are Linux" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
checkout=$(cd "$checkout" && pwd -P)
here=$checkout/script/hosted-host
[ -f "$here/hosted_host.py" ] || { echo "$checkout is not a motor checkout" >&2; exit 1; }

problems=0
skipped=0
fail() { echo "FAIL: $*" >&2; problems=$((problems + 1)); }
ok() { echo "ok: $*"; }
render() { python3 "$here/hosted_host.py" "$@"; }
# One field of an installed JSON config; for "listen", just its port.
installed() {
  python3 -c '
import json, sys
value = json.load(open(sys.argv[1]))[sys.argv[2]]
print(value.rsplit(":", 1)[1] if sys.argv[2] == "listen" else value)' "$1" "$2"
}
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# The mode string stat prints for a file: owner:group octal-mode.
attrs() { stat -c '%U:%G %a' "$1"; }

# Install one file, or report whether it is installed. kind is "config" (diff and
# ask before replacing) or "binary" (replace a different build without asking).
place() {
  local kind=$1 src=$2 dest=$3 owner=$4 mode=$5
  if [ -f "$dest" ] && [ ! -L "$dest" ] && cmp -s "$src" "$dest"; then
    if [ "$(attrs "$dest")" = "$owner $mode" ]; then
      ok "$dest"
    elif [ "$check" = 1 ]; then
      fail "$dest is $(attrs "$dest"), expected $owner $mode"
    else
      chown "$owner" "$dest" && chmod "$mode" "$dest" && ok "$dest (ownership and mode fixed)"
    fi
    return
  fi
  if [ "$check" = 1 ]; then
    if [ -e "$dest" ] && [ "$kind" = config ]; then diff -u "$dest" "$src" || true; fi
    fail "$dest is $( [ -e "$dest" ] && echo 'different from this checkout' || echo missing )"
    return
  fi
  if [ "$kind" = config ]; then
    echo "== $dest"
    if [ -e "$dest" ]; then diff -u "$dest" "$src" || true; else echo "(new file)"; fi
    local answer=
    read -r -p "Install $dest? [y/N] " answer || true
    if [ "$answer" != y ]; then
      echo "skipped $dest"
      skipped=$((skipped + 1))
      return
    fi
  fi
  [ -e "$dest" ] && cp -p "$dest" "$dest.bak-$(date -u +%Y%m%dT%H%M%SZ)"
  install -o "${owner%%:*}" -g "${owner##*:}" -m "$mode" "$src" "$dest"
  echo "installed $dest"
}

# Create a directory with an exact owner and mode, or report that it is so.
directory() {
  local path=$1 owner=$2 mode=$3
  if [ -d "$path" ] && [ ! -L "$path" ] && [ "$(attrs "$path")" = "$owner $mode" ]; then
    ok "$path/"
  elif [ "$check" = 1 ]; then
    fail "$path/ is $( [ -d "$path" ] && attrs "$path" || echo missing ), expected $owner $mode"
  else
    install -d -o "${owner%%:*}" -g "${owner##*:}" -m "$mode" "$path"
    ok "$path/ created or corrected"
  fi
}

preflight() {
  for tool in python3 ufw runuser readelf systemctl findmnt; do
    command -v "$tool" >/dev/null || fail "missing $tool"
  done
  local version
  for name in firecracker jailer; do
    version=$("$FIRECRACKER_DIR/$name" --version 2>/dev/null || true)
    if [[ "$version" == *1.17.0* ]]; then
      ok "$FIRECRACKER_DIR/$name is 1.17.0"
    else
      fail "$FIRECRACKER_DIR/$name is missing or not 1.17.0"
    fi
  done
  # The storage view is installed by the VM storage step and must already be a
  # private bind mount; this script never changes it.
  # The mount unit's installed name must be the systemd escape of its path,
  # which contains a backslash; the checkout keeps it under a portable name
  # because Windows checkouts cannot hold a backslash in a file name.
  local source unit
  for pair in 'motor-vm-storage.mount:var-lib-motor\x2dvm.mount' \
    motor-vm-storage-private.service:motor-vm-storage-private.service; do
    source=${pair%%:*}
    unit=${pair#*:}
    if cmp -s "$checkout/crates/motor-vm-helper/systemd/$source" "$UNIT_DIR/$unit"; then
      ok "$UNIT_DIR/$unit matches the checkout"
    else
      fail "$UNIT_DIR/$unit is missing or differs; see docs/linux-microvm.md#install"
    fi
  done
  if mountpoint -q "$STORAGE" && [ "$(findmnt -n -o PROPAGATION "$STORAGE")" = private ]; then
    ok "$STORAGE is a private mount"
  else
    fail "$STORAGE is not a mounted private view"
  fi
  local image
  if [ -n "$tenant_image" ]; then
    image=$tenant_image
  else
    image=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["image_version"])' "$here/tenant-1.json")
  fi
  if [ -f "$STORAGE/tenant-images/$image/manifest.json" ]; then
    ok "tenant image $image is present"
  else
    fail "tenant image $STORAGE/tenant-images/$image is missing"
  fi
  if [ -f "$TENANT_REGISTRY" ] && [ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["image_version"])' "$TENANT_REGISTRY")" = "$image" ]; then
    ok "tenant image $image matches the helper registry"
  else
    fail "$TENANT_CONFIG image does not match $TENANT_REGISTRY"
  fi
}

build_binaries() {
  local owner
  owner=$(stat -c %U "$checkout")
  [ "$owner" != root ] || { echo "refusing to build a root-owned checkout as root" >&2; exit 1; }
  echo "building as $owner in $checkout (target dir $target_dir)"
  # shellcheck disable=SC2016 # expanded by the inner shell from its arguments
  runuser -u "$owner" -- bash -c '
    set -euo pipefail
    [ -f /srv/build/runner.env ] && source /srv/build/runner.env
    export CARGO_TARGET_DIR=$2
    cd "$1"
    cargo build --release --locked -p motor-host -p motor-vm-helper
    cargo build --release --locked -p motor-vm-loader --target "$3"
  ' _ "$checkout" "$target_dir" "$LOADER_TARGET"
}

account() {
  if id -u "$HOST_USER" >/dev/null 2>&1; then
    local uid shell
    uid=$(id -u "$HOST_USER")
    shell=$(getent passwd "$HOST_USER" | cut -d: -f7)
    if [ "$uid" -gt 0 ] && [ "$uid" -lt 1000 ] && [ "$shell" = /usr/sbin/nologin ]; then
      ok "system account $HOST_USER ($uid)"
    else
      fail "$HOST_USER exists but is not a nologin system account (uid $uid, shell $shell)"
    fi
  elif [ "$check" = 1 ]; then
    fail "account $HOST_USER is missing"
  else
    useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$HOST_USER"
    ok "created system account $HOST_USER ($(id -u "$HOST_USER"))"
  fi
}

binaries() {
  local release=$target_dir/release loader=$target_dir/$LOADER_TARGET/release/motor-vm-loader
  for built in "$release/motor-host" "$release/motor-vm-helper" "$loader"; do
    [ -x "$built" ] || { fail "missing build output $built; run with --build or build it first"; return; }
  done
  # The loader runs in an empty root: a dynamic binary would find no libc there.
  # (Read the headers into a file first: grep -q closing a pipe early would turn
  # readelf's SIGPIPE into a pipefail status and hide a match.)
  readelf -lW "$loader" > "$scratch/loader-headers" || { fail "$loader is not an ELF binary"; return; }
  if grep -q 'program interpreter' "$scratch/loader-headers"; then
    fail "$loader is dynamically linked; build it for $LOADER_TARGET"
    return
  fi
  if [ "$check" = 0 ] && [ ! -d /usr/local/libexec ]; then
    install -d -o root -g root -m 755 /usr/local/libexec
  fi
  place binary "$release/motor-host" "$HOST_BINARY" root:root 755
  place binary "$release/motor-vm-helper" "$HELPER_BINARY" root:root 755
  place binary "$loader" "$LOADER" root:root 755
}

configs() {
  local uid gid
  uid=$(id -u "$HOST_USER" 2>/dev/null) || { fail "cannot render configs without $HOST_USER"; return; }
  gid=$(id -g "$HOST_USER")
  directory /etc/motor-host root:root 755
  directory /etc/motor-host/provision root:root 755
  directory "$KEY_DIR" "$HOST_USER:$HOST_USER" 700
  directory "$JOURNAL_DIR" "$HOST_USER:$HOST_USER" 700
  render host --host-id "$host_id" --port "$port" > "$scratch/host.json" || { fail "cannot render host config"; return; }
  # An installed tenant record is rendered back as itself and gives the helper
  # its slot CPUs; only a first install renders the tenant-1.json template.
  render helper --uid "$uid" --gid "$gid" ${installed_record:+--installed "$installed_record"} \
    > "$scratch/helper.json" || { fail "cannot render helper config"; return; }
  render tenant ${tenant_sub:+--sub "$tenant_sub"} ${tenant_quota:+--disk-quota-gib "$tenant_quota"} \
    ${installed_record:+--installed "$installed_record"} \
    > "$scratch/tenant.json" || { fail "cannot render tenant config"; return; }
  place config "$scratch/host.json" "$HOST_CONFIG" root:root 644
  place config "$scratch/helper.json" "$HELPER_CONFIG" root:root 644
  place config "$scratch/tenant.json" "$TENANT_CONFIG" root:root 644
  if [ "$check" = 1 ]; then
    if render check-installed --uid "$uid" --gid "$gid"; then
      ok "installed tenant state"
    else
      fail "installed tenant state (problems listed above)"
    fi
  fi
  # motor-host refuses to start without a binding map. provision.py adds the
  # tenant to it; an existing map is never touched here.
  if [ -f "$BINDINGS" ]; then
    ok "$BINDINGS exists ($(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$BINDINGS") bindings)"
  elif [ "$check" = 1 ]; then
    fail "$BINDINGS is missing"
  else
    printf '{}\n' > "$scratch/bindings.json"
    install -o root -g root -m 644 "$scratch/bindings.json" "$BINDINGS"
    ok "$BINDINGS created empty"
  fi
}

units() {
  place config "$checkout/crates/motor-vm-helper/systemd/motor-vm-helper.service" "$UNIT_DIR/motor-vm-helper.service" root:root 644
  place config "$checkout/crates/motor-host/systemd/motor-host.service" "$UNIT_DIR/motor-host.service" root:root 644
  if [ "$check" = 0 ]; then
    systemctl daemon-reload || fail "systemctl daemon-reload failed"
  fi
  for unit in motor-vm-helper.service motor-host.service; do
    echo "status: $unit $(systemctl is-enabled "$unit" 2>/dev/null || true) $(systemctl is-active "$unit" 2>/dev/null || true)"
  done
}

keys() {
  local present=0
  for file in "$SIGNING_KEY" "$TLS_CERT" "$TLS_KEY"; do
    if [ -e "$file" ] || [ -L "$file" ]; then
      present=$((present + 1))
      [ "$(attrs "$file")" = "$HOST_USER:$HOST_USER 600" ] || fail "$file is $(attrs "$file"), expected $HOST_USER:$HOST_USER 600"
    fi
  done
  if [ "$present" = 3 ]; then
    ok "host keys exist (never replaced)"
  elif [ "$present" != 0 ]; then
    fail "only $present of the 3 host key files exist; investigate before generating new keys"
  elif [ "$check" = 1 ]; then
    fail "host keys are missing"
  else
    if (cd / && runuser -u "$HOST_USER" -- "$HOST_BINARY" keygen --signing-key "$SIGNING_KEY" \
      --tls-cert "$TLS_CERT" --tls-key "$TLS_KEY" --host-id "$host_id" --endpoint "$public_ip:$port") >/dev/null; then
      ok "generated host keys"
    else
      fail "keygen failed"
    fi
  fi
}

# The inbound policy holds: SSH, plus (when given) the agent's port. The rule
# itself is added by --start and removed by --stop.
firewall() {
  if python3 "$checkout/script/runner/inbound_policy.py" ${1:+--host-agent-port "$1"}; then
    ok "inbound policy: SSH${1:+ and $1/tcp (IPv4)} only"
  else
    fail "inbound policy check failed"
  fi
}

enrollment() {
  echo "== enrollment facts for the broker's register-host"
  (cd / && runuser -u "$HOST_USER" -- "$HOST_BINARY" show-enrollment "$HOST_CONFIG" --endpoint "$public_ip:$port") \
    || fail "show-enrollment failed"
}

start_units() {
  local bound
  bound=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))))' "$BINDINGS")
  if [ "$bound" = 0 ]; then
    echo "refusing --start: $BINDINGS has no tenant; provision one with script/tenant-provision/provision.py first" >&2
    exit 1
  fi
  # The one inbound exception besides SSH: IPv4 TCP to the agent's port, named
  # motor-host-agent. script/runner/inbound_policy.py (also run by setup.sh
  # with RUNNER_HOST_AGENT_PORT) accepts exactly this rule.
  ufw allow in proto tcp from any to 0.0.0.0/0 port "$port" comment motor-host-agent
  python3 "$checkout/script/runner/inbound_policy.py" --host-agent-port "$port"
  systemctl enable --now motor-vm-helper.service
  for _ in $(seq 1 50); do [ -S "$HELPER_SOCKET" ] && break; sleep 0.2; done
  [ -S "$HELPER_SOCKET" ] || { systemctl status --no-pager motor-vm-helper.service || true; echo "helper socket did not appear" >&2; exit 1; }
  systemctl enable --now motor-host.service
  sleep 2
  systemctl is-active motor-host.service || { journalctl -u motor-host.service -n 50 --no-pager; exit 1; }
  echo "started: motor-vm-helper.service and motor-host.service; the host is placeable after its first heartbeat"
}

stop_units() {
  # Stop the agent first so it sends no further heartbeat, then the helper,
  # which stops the tenant VM cleanly before it exits. Nothing on disk is removed.
  systemctl disable --now motor-host.service || fail "cannot stop motor-host.service"
  systemctl disable --now motor-vm-helper.service || fail "cannot stop motor-vm-helper.service"
  # Delete the agent rule by number, highest first so earlier numbers stay valid.
  local numbers
  numbers=$(ufw status numbered | sed -n "s/^\[ *\([0-9]*\)\] $port\/tcp .*# motor-host-agent\$/\1/p" | sort -rn)
  for number in $numbers; do
    ufw --force delete "$number" || fail "cannot delete firewall rule $number"
  done
  firewall ""
  if [ "$problems" != 0 ]; then
    echo "$problems problem(s) while stopping" >&2
    exit 1
  fi
  echo "stopped: both units disabled and stopped, port $port closed"
}

if [ "$mode" != install ]; then
  for file in "$HOST_CONFIG" "$TENANT_CONFIG"; do
    [ -f "$file" ] || { echo "--$mode needs the installed $file; install first" >&2; exit 1; }
  done
  port=$(installed "$HOST_CONFIG" listen)
  host_id=$(installed "$HOST_CONFIG" host_id)
  tenant_sub=$(installed "$TENANT_CONFIG" sub)
fi
# The installed tenant record, when there is one, is the source of the
# tenant's sizing and image when installing, checking and starting.
installed_record=''
if [ "$mode" != stop ] && [ -f "$TENANT_CONFIG" ]; then
  installed_record=$TENANT_CONFIG
  tenant_image=$(installed "$TENANT_CONFIG" image_version)
fi
port=${port:-8443}
[[ "$port" =~ ^[1-9][0-9]{3,4}$ ]] || { echo "invalid port $port" >&2; exit 2; }
if [ "$mode" = stop ]; then
  stop_units
  exit 0
fi

[ -n "$public_ip" ] || public_ip=$(render public-ip)
echo "hosted host $host_id endpoint $public_ip:$port ($mode)"
[ "$build" = 1 ] && build_binaries
preflight
account
binaries
configs
units
keys
firewall "$port"
enrollment
if [ "$problems" != 0 ] || [ "$skipped" != 0 ]; then
  echo "$problems problem(s), $skipped declined change(s); not complete" >&2
  exit 1
fi
if [ "$mode" = start ]; then
  start_units
else
  echo "hosted host $( [ "$mode" = check ] && echo verified || echo installed ); nothing was enabled or started"
fi
