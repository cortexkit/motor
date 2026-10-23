#!/usr/bin/env bash
# Run before deploying a runner change to nested JavaScript installs, to show
# every fleet repository's nested lockfiles still install under the new runner.
# Run on the operator machine. For every git checkout directly under the
# CortexKit projects directory, ship its default branch to the build server and
# run motor-runner's own install command in each tracked nested directory that
# holds package.json plus a supported lockfile, shallowest first. The root is
# skipped: the runner already installs it today. Prints one PASS or FAIL line per
# directory. Nothing on the operator machine changes; the server copies are
# deleted at the end.
set -euo pipefail

projects=${1:-$HOME/Work/Projects/CortexKit}
host=${RUNNER_HOST:-alfonso-runner}
remote=/srv/build/review/motor-js-rollout
here=$(cd -- "$(dirname -- "$0")" && pwd)
failed=0

ssh -o BatchMode=yes "$host" "rm -rf $remote && mkdir -p $remote"
trap 'ssh -o BatchMode=yes "$host" "rm -rf $remote"' EXIT
scp -q "$here/check_nested_js_remote.sh" "$host:$remote/run.sh"

for repo in "$projects"/*/; do
  repo=${repo%/}
  top=$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null) || continue
  [ "$top" = "$repo" ] || continue
  branch=$(git -C "$repo" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null) ||
    branch=$(git -C "$repo" rev-parse --abbrev-ref HEAD)
  commit=$(git -C "$repo" rev-parse "$branch")
  dirs=$(git -C "$repo" ls-tree -r --name-only "$commit" |
    grep -vE '(^|/)node_modules/' |
    grep -E '(^|/)(bun\.lock|bun\.lockb|package-lock\.json|pnpm-lock\.yaml)$' |
    xargs -r -n1 dirname | grep -vx '\.' | sort -u |
    while read -r d; do
      if git -C "$repo" cat-file -e "$commit:$d/package.json" 2>/dev/null; then
        echo "$d"
      fi
    done | awk -F/ '{print NF "\t" $0}' | sort -k1,1n -k2,2 | cut -f2) || true
  [ -n "$dirs" ] || continue
  name=$(basename "$repo")
  tmp=$(mktemp -d)
  git -C "$repo" bundle create -q "$tmp/$name.bundle" "$commit" 2>/dev/null ||
    git -C "$repo" bundle create -q "$tmp/$name.bundle" "$branch"
  scp -q "$tmp/$name.bundle" "$host:$remote/$name.bundle"
  rm -rf "$tmp"
  if ! printf '%s\n' "$dirs" | ssh -o BatchMode=yes "$host" bash "$remote/run.sh" "$remote" "$name" "$commit"; then
    failed=1
  fi
done
exit "$failed"
