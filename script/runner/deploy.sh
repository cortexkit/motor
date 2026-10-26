#!/usr/bin/env bash
# Run as the Linux service owner; deployment must never rerun accepted commands.
set -euo pipefail
if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "Usage: $0 /absolute/path/to/candidate [instance]" >&2
  exit 2
fi
candidate=$1
instance=${2:-main}
case "$instance" in *[!a-zA-Z0-9_-]*|'') echo 'Invalid instance' >&2; exit 2;; esac
unit=motor-runner.service
live=${RUNNER_LIVE_BINARY:-$HOME/.cargo/bin/motor-runner}
if [ "$instance" != main ]; then
  unit="motor-runner@$instance.service"
  live=${RUNNER_LIVE_BINARY:-$HOME/.cargo/bin/motor-runner-$instance}
fi
deadline=${RUNNER_DRAIN_TIMEOUT_S:-600}
state_root=${RUNNER_STATE_ROOT:-${XDG_DATA_HOME:-$HOME/.local/share}/cortexkit/motor-runner}
if [ "$instance" = windows ]; then
  state_root=${RUNNER_STATE_ROOT:-/srv/build/motor-windows/windows/state}
fi
script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
test -x "$candidate"
"$candidate" --version
if helper_api=$("$candidate" permission-helper-digest 2>&1); then
  "$candidate" check-permissions-helper
else
  case "$helper_api" in
    *'usage: motor-runner serve|connect'*) ;; # Older candidates do not use this helper.
    *) printf '%s\n' "$helper_api" >&2; echo 'Candidate helper capability probe failed' >&2; exit 1;;
  esac
fi
# Query the installed unit, never a repository unit that may not be installed yet.
unit_text=$(systemctl --user cat "$unit")
mapfile -d '' -t unit_command < <(printf '%s\n' "$unit_text" |
  python3 "$script_dir/deploy_canary.py" --unit-command "$instance")
[ "${#unit_command[@]}" -gt 0 ] || { echo "Cannot extract serve arguments for $unit" >&2; exit 1; }
serve_options=("${unit_command[@]:1}")
caller_roots=false
for ((i=0; i<${#serve_options[@]}; i++)); do
  case "${serve_options[i]}" in
    --caller-root|--caller-root=*) caller_roots=true ;;
    --state-root)
      if [ "$((i+1))" -ge "${#serve_options[@]}" ]; then
        echo "Deploy aborted: $unit has --state-root without a value" >&2; exit 1
      fi
      state_root=${serve_options[i+1]} ;;
    --state-root=*) state_root=${serve_options[i]#--state-root=} ;;
  esac
done
if options_check=$("$candidate" check-options "${serve_options[@]}" 2>&1); then
  :
elif [[ "$options_check" = *'usage: motor-runner '* && "$options_check" != *check-options* ]]; then
  if [ "$caller_roots" = true ] || [ -e "$state_root/$instance/caller-roots-ever-enabled" ]; then
    printf 'Deploy aborted: candidate %s lacks check-options; downgrade to a pre-mapping binary is unsupported after caller roots were enabled or with --caller-root.\n%s\n' "$candidate" "$options_check" >&2
    exit 1
  fi
else
  printf 'Deploy aborted: candidate %s check-options refused installed arguments:\n%s\n' "$candidate" "$options_check" >&2
  exit 1
fi
# Stage without touching the executable inode of the currently serving daemon.
install -m 755 "$candidate" "$live.new"
admission_held=false
stopped=false
rollback=
warn_quarantine() {
  local folder="$state_root/$instance/quarantine" contents
  [ -d "$folder" ] || return 0
  if contents=$(ls -A -- "$folder" 2>&1); then
    if [ -n "$contents" ]; then
      printf 'WARNING: runner quarantine is not empty: %s\n%s\n' "$folder" "$contents" >&2
    fi
  else
    printf 'WARNING: could not inspect runner quarantine: %s\n%s\n' "$folder" "$contents" >&2
  fi
}
cleanup() {
  result=$?
  trap - EXIT
  if [ "$result" -ne 0 ] && [ "$stopped" = true ]; then
    if [ "$caller_roots" = true ]; then
      echo 'Executable rollback does not remove --caller-root. For flag-off rollback: stop the instance; run forget-caller-root with its flagged serve arguments; install the unflagged unit with install-units.sh; systemctl --user start the instance; then deploy.sh with a copy of the installed binary. See docs/runner-install.md.' >&2
    fi
    # Stop the failed candidate, including a daemon that started but failed its
    # job canary. Replace by rename, never write a running executable's inode.
    if systemctl --user stop "$unit" && cp -p "$rollback" "$live.new" &&
       mv "$live.new" "$live" && systemctl --user start "$unit" &&
       systemctl --user is-active "$unit"; then
      stopped=false
      echo "Deployment failed; restored rollback executable $rollback" >&2
    else
      echo "Rollback failed; manually restore $rollback and start $unit" >&2
    fi
    warn_quarantine
  fi
  if [ "$admission_held" = true ] && [ "$stopped" = false ]; then
    admin="$live.new"
    [ -x "$admin" ] || admin="$live"
    if ! "$admin" resume --instance "$instance" --state-root "$state_root"; then
      echo "Could not reopen admission (older runners may not support Resume). No signal was sent." >&2
      echo "Manual recovery: finish the named survivors, then systemctl --user restart $unit" >&2
      result=1
    fi
  fi
  rm -f "$live.new"
  exit "$result"
}
trap cleanup EXIT
# Hold admission closed after the acknowledgement until systemctl stop. This
# avoids a Restart=always service admitting jobs between drain and the swap.
admission_held=true
if drain_output=$("$live.new" drain --instance "$instance" --state-root "$state_root" --drain-timeout-s "$deadline" --exit-after-drain false 2>&1); then
  printf '%s\n' "$drain_output"
else
  printf '%s\n' "$drain_output" >&2
  # Linux recovery can inspect recorded systemd units and close records that
  # prove no command started. Started Windows jobs cannot reattach after restart
  # and must finish on the old daemon, as must uncertain Linux survivors.
  # Missing/unreadable records fail closed. Admission remains held during this
  # inventory; on any abort the EXIT trap requests Resume, never a signal.
  case "$drain_output" in
    *'drain deadline exceeded; surviving jobs will be recovered on restart'*)
      if python3 "$script_dir/drain_survivors.py" "$state_root/$instance"; then
        echo 'Drain timed out; all survivors are restart-recoverable. Proceeding to swap.' >&2
      else
        echo 'Deploy aborted: unsafe survivors listed above; reopening old daemon admission.' >&2
        exit 1
      fi
      ;;
    *) echo 'Deploy aborted: drain was not acknowledged; reopening old daemon admission.' >&2; exit 1;;
  esac
fi
stamp=$(date -u +%Y%m%dT%H%M%SZ)
rollback="$live.rollback-$stamp"
test ! -e "$rollback"
cp -p "$live" "$rollback"
systemctl --user stop "$unit"
stopped=true
mv "$live.new" "$live"
systemctl --user start "$unit"
systemctl --user is-active "$unit"
if [ "$instance" = windows ]; then
  echo 'Post-swap isolated canary explicitly skipped for windows (requires a qualified Windows VM).' >&2
else
  canary_modes=(/Users)
  [ "$caller_roots" = false ] || canary_modes+=(/home)
  for mode in "${canary_modes[@]}"; do
    canary_options=()
    [ "$mode" != /home ] || canary_options+=(--home-compile-check)
    if python3 "$script_dir/deploy_canary.py" --state-root "$state_root" --instance "$instance" \
        --timeout-s "${RUNNER_CANARY_TIMEOUT_S:-120}" "${canary_options[@]}"; then
      :
    else
      canary_status=$?
      if [ "$canary_status" -eq 75 ]; then
        echo "canary inconclusive: no free permit; keeping the new runner (isolation not verified). Mode: $mode" >&2
      else
        echo "Post-swap isolated canary failed; rolling back the runner. Mode: $mode" >&2
        exit 1
      fi
    fi
  done
fi
stopped=false
admission_held=false
warn_quarantine
"$live" --version
printf 'deployed %s; rollback executable: %s\n' "$unit" "$rollback"
