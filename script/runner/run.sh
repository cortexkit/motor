#!/usr/bin/env bash
# Operator tool: sync a source copy and run a command by hand.
# Normal remote jobs use the ck-motor execution service instead.
# One checkout and cache: lock both sync and execution, not just cargo's cache.
set -euo pipefail
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
  echo "Usage: $0 '<command>'" >&2
  exit 2
fi
host=${RUNNER_HOST:-alfonso-runner}
script_dir=$(cd "$(dirname "$0")" && pwd)
root=${RUNNER_REPO_ROOT:-$(cd "$script_dir/../.." && pwd)}
root=$(cd "$root" && pwd -P)
name=${root##*/}
[[ "$name" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "Unsafe repository name: $name" >&2; exit 2; }
ssh -o BatchMode=yes -o ForwardAgent=no "$host" 'mkdir /srv/build/runner.lock' || {
  echo 'Runner busy (or not set up); check /srv/build and the runner.lock owner before retrying' >&2
  exit 1
}
trap 'ssh -o BatchMode=yes -o ForwardAgent=no "$host" "rmdir /srv/build/runner.lock" || true' EXIT
RUNNER_LOCK_HELD=1 "$script_dir/sync.sh" "$root"
printf -v quoted '%q' "$1"
ssh -o BatchMode=yes -o ForwardAgent=no "$host" "bash -s -- $name $quoted" <<'REMOTE'
set -euo pipefail
source /srv/build/runner.env
cd "/srv/build/src/$1"
exec bash -c "$2"
REMOTE
