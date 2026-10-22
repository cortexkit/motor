#!/usr/bin/env bash
# Measure what one sibling snapshot of a repository would cost on disk.
#
# A sibling snapshot is a read-only copy of a repository checkout at one
# commit: its working tree, its JavaScript installs and a self-contained .git,
# but no target/ directory (see the design notes on sibling snapshots, section
# Q4). Before the snapshot cap is chosen, the owner runs this on the build
# server for commons, subconscious, insula and broca and records the output.
#
# Usage: scripts/sibling-snapshot-sizes.sh <checkout>...
#
# For each checkout P it prints, in KiB of allocated disk (GNU `du -sk`):
#   - P without any entry named target       du -sk --exclude=target P
#   - P/.git                                 du -sk P/.git
#   - each node_modules directory under P    du -sk <dir>
# The first figure already includes .git and node_modules; the others show how
# much of it they are. Nested node_modules inside another node_modules, and any
# under a target directory, are not listed separately.
#
# The report starts with the UTC date and the host name, and each checkout's
# block names its HEAD commit, so a recorded measurement says when, where and at
# which revision it was taken. It reads only; it never changes a checkout.
#
# Requires GNU du (for --exclude); the build server's Linux has it.

set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <checkout>..." >&2
  exit 2
fi

if ! du --version 2>/dev/null | grep -q 'GNU coreutils'; then
  echo "GNU du is required (for --exclude); run this on the Linux build server" >&2
  exit 2
fi

kib() {
  du -sk "$@" | cut -f1
}

echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "host: $(hostname)"

status=0
for checkout in "$@"; do
  echo
  if [ ! -d "$checkout/.git" ]; then
    echo "$checkout: not a git checkout (no .git directory)" >&2
    status=1
    continue
  fi
  path="$(cd "$checkout" && pwd -P)"
  commit="$(git -C "$path" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "checkout: $path"
  echo "commit: $commit"
  echo "without target (KiB): $(kib --exclude=target "$path")"
  echo ".git (KiB): $(kib "$path/.git")"
  found=0
  # -prune stops the walk at the first node_modules and at every target, so
  # each listed directory is a top-level install and none is a build output.
  while IFS= read -r -d '' modules; do
    found=1
    echo "${modules#"$path"/} (KiB): $(kib "$modules")"
  done < <(find "$path" \( -name target -o -name .git \) -prune -o \
    -type d -name node_modules -prune -print0 | sort -z)
  if [ "$found" -eq 0 ]; then
    echo "node_modules: none"
  fi
done
exit "$status"
