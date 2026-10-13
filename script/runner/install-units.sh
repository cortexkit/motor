#!/usr/bin/env bash
# Install the runner's systemd user units from script/runner/units/ on the build
# server, as the operator (ubuntu). The repository copy is the source of truth:
# capacity flags such as --max-weight live in units/motor-runner.service.
#
# This only writes the unit files and reloads systemd. It never restarts a
# runner: a changed ExecStart takes effect at the next deploy.sh swap, which
# drains first. Shows a diff and asks before overwriting a file that differs.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
dest=$HOME/.config/systemd/user
changed=0
# The file list comes in on fd 3, so the y/N prompt below reads the operator's
# own stdin instead of consuming the next file name.
while IFS= read -r -d '' -u 3 file; do
  rel=${file#"$here/units/"}
  target=$dest/$rel
  if [ -f "$target" ] && cmp -s "$file" "$target"; then
    continue
  fi
  echo "== $rel"
  if [ -f "$target" ]; then
    diff -u "$target" "$file" || true
  else
    echo "(new file)"
  fi
  read -r -p "Install $rel? [y/N] " answer
  [ "$answer" = y ] || { echo "skipped $rel"; continue; }
  mkdir -p "$(dirname "$target")"
  [ -f "$target" ] && cp -p "$target" "$target.bak-$(date -u +%Y%m%dT%H%M%SZ)"
  install -m 0644 "$file" "$target"
  changed=1
done 3< <(find "$here/units" -type f -print0 | sort -z)
if [ "$changed" = 1 ]; then
  systemctl --user daemon-reload
  echo "Units installed. Restart a runner only through deploy.sh."
else
  echo "Installed units already match the repository."
fi
