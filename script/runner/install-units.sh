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
check_instance() {
  local instance=$1 binary override refusal arg flagged=false
  local -a command options
  # The effective ExecStart selects each instance's binary, not the template's binary.
  mapfile -d '' -t command < <(
    python3 "$here/deploy_canary.py" --repository-unit-command "$here/units" "$instance")
  [ "${#command[@]}" -gt 0 ] || { echo "skipped $rel: cannot extract $instance serve arguments" >&2; return 1; }
  options=("${command[@]:1}")
  for arg in "${options[@]}"; do
    case "$arg" in --caller-root|--caller-root=*) flagged=true;; esac
  done
  override=RUNNER_BINARY_$instance
  binary=${!override:-${command[0]}}
  if [ ! -x "$binary" ]; then
    [ "$flagged" = false ] && return 0 # Preserve first installation of unflagged units.
    refusal="binary is absent or not executable: $binary"
  elif refusal=$("$binary" check-options "${options[@]}" 2>&1); then
    return 0
  elif [[ "$refusal" = *'usage: motor-runner '* && "$refusal" != *check-options* ]]; then
    [ "$flagged" = false ] && return 0
    refusal="binary $binary lacks check-options: $refusal"
  fi
  printf 'skipped %s: %s check-options refusal: %s\n' "$rel" "$instance" "$refusal" >&2
  return 1
}
# The file list comes in on fd 3, so the y/N prompt below reads the operator's
# own stdin instead of consuming the next file name.
while IFS= read -r -d '' -u 3 file; do
  rel=${file#"$here/units/"}
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
