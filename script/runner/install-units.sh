#!/usr/bin/env bash
# Install the runner's systemd user units from script/runner/units/ on the build
# server, as the operator (ubuntu). The repository copy is the source of truth:
# capacity flags such as --max-weight live in units/motor-runner.service.
#
# This writes user units and privileged memory-policy units and reloads systemd. It never restarts a
# runner: a changed ExecStart takes effect at the next deploy.sh swap, which
# drains first. Shows a diff and asks before overwriting a file that differs.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dest=$HOME/.config/systemd/user
changed=0
metrics_endpoint='' metrics_host_id='' metrics_key=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --metrics-endpoint|--metrics-host-id)
      [ "$#" -ge 2 ] && [ -n "$2" ] || { echo "$1 requires a value" >&2; exit 1; }
      case "$1" in
        --metrics-endpoint) metrics_endpoint=$2 ;;
        --metrics-host-id) metrics_host_id=$2 ;;
      esac
      shift 2 ;;
    --metrics-key) metrics_key=true; shift ;;
    --help)
      echo 'Usage: install-units.sh [--metrics-endpoint HTTPS_ORIGIN --metrics-host-id ID] [--metrics-key]'
      echo 'Metrics apply only to motor-runner@windows, at its next planned restart.'
      echo '--metrics-key prints its public key without installing units or starting a service.'
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done
python3 - "$metrics_endpoint" "$metrics_host_id" <<'PY'
import re
import sys
from urllib.parse import urlsplit
endpoint, host_id = sys.argv[1:]
if bool(endpoint) != bool(host_id):
    sys.exit('metrics endpoint and host id must be supplied together')
if endpoint:
    try:
        url = urlsplit(endpoint)
        valid = (re.fullmatch(r'https://[A-Za-z0-9.\-:\[\]]+/?', endpoint) and
                 url.hostname and url.path in ('', '/') and
                 (url.port is None or 1 <= url.port <= 65535))
    except ValueError:
        valid = False
    if not valid:
        sys.exit('metrics endpoint must be an HTTPS origin')
    if not re.fullmatch(r'[A-Za-z0-9._-]{1,64}', host_id):
        sys.exit('metrics host id must match [A-Za-z0-9._-]{1,64}')
PY
metrics_args=()
if [ -n "$metrics_endpoint" ]; then
  metrics_args=(--metrics-push --metrics-endpoint "$metrics_endpoint" --metrics-host-id "$metrics_host_id")
fi
if [ "$metrics_key" = true ]; then
  mapfile -d '' -t command < <(
    python3 "$here/deploy_canary.py" --repository-unit-command "$here/units" windows)
  [ "${#command[@]}" -gt 0 ] || { echo 'Cannot extract Windows runner command' >&2; exit 1; }
  identity=()
  for ((i=1; i<${#command[@]}; i++)); do
    case "${command[i]}" in
      --instance|--state-root) identity+=("${command[i]}" "${command[i+1]}"); ((i+=1)) ;;
      --instance=*|--state-root=*) identity+=("${command[i]}") ;;
    esac
  done
  exec "${RUNNER_BINARY_windows:-${command[0]}}" metrics-key "${identity[@]}"
fi
rendered=
trap '[ -z "$rendered" ] || rm -f "$rendered"' EXIT
check_instance() {
  local instance=$1 binary override refusal state_root i flagged=false ever_enabled=false
  local -a command options
  # The effective ExecStart selects each instance's binary, not the template's binary.
  mapfile -d '' -t command < <(
    python3 "$here/deploy_canary.py" --repository-unit-command "$here/units" "$instance")
  [ "${#command[@]}" -gt 0 ] || { echo "skipped $rel: cannot extract $instance serve arguments" >&2; return 1; }
  options=("${command[@]:1}")
  if [ "$instance" = windows ]; then options+=("${metrics_args[@]}"); fi
  state_root=${RUNNER_STATE_ROOT:-${XDG_DATA_HOME:-$HOME/.local/share}/cortexkit/motor-runner}
  if [ "$instance" = windows ]; then
    state_root=${RUNNER_STATE_ROOT:-/srv/build/motor-windows/windows/state}
  fi
  for ((i=0; i<${#options[@]}; i++)); do
    case "${options[i]}" in
      --caller-root|--caller-root=*) flagged=true ;;
      --state-root)
        if [ "$((i+1))" -ge "${#options[@]}" ]; then
          echo "skipped $rel: $instance has --state-root without a value" >&2; return 1
        fi
        state_root=${options[i+1]} ;;
      --state-root=*) state_root=${options[i]#--state-root=} ;;
    esac
  done
  # Removing the flag does not erase mapping history: later starts must still
  # use a binary that understands the persistent caller-root state.
  [ ! -e "$state_root/$instance/caller-roots-ever-enabled" ] || ever_enabled=true
  override=RUNNER_BINARY_$instance
  binary=${!override:-${command[0]}}
  if [ ! -x "$binary" ]; then
    [ "$flagged" = false ] && [ "$ever_enabled" = false ] && return 0 # Allow unflagged units with no mapping history.
    refusal="binary is absent or not executable: $binary; cannot verify caller-root compatibility"
  elif refusal=$("$binary" check-options "${options[@]}" 2>&1); then
    return 0
  elif [[ "$refusal" = *'usage: motor-runner '* && "$refusal" != *check-options* ]]; then
    [ "$flagged" = false ] && [ "$ever_enabled" = false ] && return 0
    refusal="binary $binary lacks check-options: $refusal"
  fi
  if [ "$ever_enabled" = true ]; then
    refusal="$refusal; caller roots were enabled for $instance; a pre-mapping binary is unsupported"
  fi
  printf 'skipped %s: %s check-options refusal: %s\n' "$rel" "$instance" "$refusal" >&2
  return 1
}
# The file list comes in on fd 3, so the y/N prompt below reads the operator's
# own stdin instead of consuming the next file name.
while IFS= read -r -d '' -u 3 file; do
  rel=${file#"$here/units/"}
  if [ "$rel" = motor-runner@windows.service.d/windows.conf ] && [ "${#metrics_args[@]}" -gt 0 ]; then
    rendered=$(mktemp "${TMPDIR:?TMPDIR must name the shared scratch root}/motor-units.XXXXXX")
    while IFS= read -r line; do
      if [[ "$line" = ExecStart=*' serve '* ]]; then line+=" ${metrics_args[*]}"; fi
      printf '%s\n' "$line"
    done < "$file" > "$rendered"
    file=$rendered
  fi
  privileged=0
  case "$rel" in
    system/motor-jobs.slice)
      target=/etc/systemd/system/motor-jobs.slice
      privileged=1 ;;
    system/runner-user-manager.conf)
      target=/etc/systemd/system/user@$(id -u).service.d/motor-oom.conf
      privileged=1 ;;
    *) target=$dest/$rel ;;
  esac
  if [ -f "$target" ] && cmp -s "$file" "$target"; then
    continue
  fi
  instances=()
  case "$rel" in
    motor-runner.service|motor-runner.service.d/*.conf) instances=(main) ;;
    motor-runner@.service|motor-runner@.service.d/*.conf) instances=(e2e windows) ;;
    motor-runner@*.service.d/*.conf)
      name=${rel#motor-runner@}; instances=("${name%%.service.d/*}") ;;
  esac
  allowed=true
  for instance in "${instances[@]}"; do
    check_instance "$instance" || allowed=false
  done
  [ "$allowed" = true ] || continue
  echo "== $rel"
  if [ -f "$target" ]; then
    diff -u "$target" "$file" || true
  else
    echo "(new file)"
    diff -u /dev/null "$file" || true
  fi
  # End of input counts as "no" for this and later files. Under set -e a
  # failing read would otherwise exit here, skipping the daemon-reload below
  # for files already installed, and the next restart would run the old unit.
  answer=
  read -r -p "Install $rel? [y/N] " answer || true
  [ "$answer" = y ] || { echo "skipped $rel"; continue; }
  if [ "$privileged" = 1 ]; then
    sudo install -d -o root -g root -m 0755 "$(dirname "$target")"
    [ ! -f "$target" ] || sudo cp -p "$target" "$target.bak-$(date -u +%Y%m%dT%H%M%SZ)"
    sudo install -o root -g root -m 0644 "$file" "$target"
  else
    mkdir -p "$(dirname "$target")"
    [ ! -f "$target" ] || cp -p "$target" "$target.bak-$(date -u +%Y%m%dT%H%M%SZ)"
    install -m 0644 "$file" "$target"
  fi
  changed=1
done 3< <(find "$here/units" -type f -print0 | sort -z)
if [ "$changed" = 1 ]; then
  sudo systemctl daemon-reload
  systemctl --user daemon-reload
  echo "Units installed. Drain all runner instances before rebooting to apply the user-manager OOM score."
  echo "Do not restart user@$(id -u).service with live jobs. Afterwards restart runners only through deploy.sh."
else
  echo "Installed units already match the repository."
fi
