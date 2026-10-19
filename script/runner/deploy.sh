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
# Stage without touching the executable inode of the currently serving daemon.
install -m 755 "$candidate" "$live.new"
admission_held=false
stopped=false
rollback=
cleanup() {
  result=$?
  trap - EXIT
  if [ "$result" -ne 0 ] && [ "$stopped" = true ]; then
    # A failed swap/start restores the old executable before reopening it.
    if cp -p "$rollback" "$live" && systemctl --user start "$unit"; then
      stopped=false
    else
      echo "Rollback failed; manually restore $rollback and start $unit" >&2
    fi
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
stopped=false
admission_held=false
"$live" --version
printf 'deployed %s; rollback executable: %s\n' "$unit" "$rollback"
