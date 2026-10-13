#!/usr/bin/env bash
# Server half of check_nested_js.sh. Reads nested directories on stdin and runs
# the same install command motor-runner uses for each lockfile, lifecycle
# scripts off, with the network the trusted setup step has.
set -u
remote=$1 name=$2 commit=$3
cd "$remote"
git init -q "$name"
git -C "$name" fetch -q "$remote/$name.bundle" "+refs/*:refs/bundle/*" 2>/dev/null
git -C "$name" checkout -q --detach "$commit" || { echo "FAIL $name: commit $commit not in bundle"; exit 0; }
while read -r dir; do
  cd "$remote/$name/$dir"
  if [ -f bun.lock ]; then lock=bun.lock; cmd=("$HOME/.bun/bin/bun" install --frozen-lockfile --ignore-scripts)
  elif [ -f bun.lockb ]; then lock=bun.lockb; cmd=("$HOME/.bun/bin/bun" install --frozen-lockfile --ignore-scripts)
  elif [ -f package-lock.json ]; then lock=package-lock.json; cmd=(npm ci --ignore-scripts)
  else lock=pnpm-lock.yaml; cmd=(pnpm install --frozen-lockfile --ignore-scripts)
  fi
  start=$(date +%s%N)
  if out=$("${cmd[@]}" 2>&1); then
    echo "PASS $name $dir/$lock $(( ($(date +%s%N) - start) / 1000000 )) ms"
  else
    echo "FAIL $name $dir/$lock: $(printf '%s\n' "$out" | grep -m1 -iE 'err|fail' | cut -c1-220)"
  fi
done
