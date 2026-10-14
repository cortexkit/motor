#!/usr/bin/env bash
# Server half of check_nested_js.sh. Reads nested directories on stdin and runs
# the same install command motor-runner uses for each lockfile, lifecycle
# scripts off, with the network the trusted setup step has.
set -u
remote=$1 name=$2 commit=$3
failed=0
cd "$remote"
git init -q "$name"
git -C "$name" fetch -q "$remote/$name.bundle" "+refs/*:refs/bundle/*" 2>/dev/null
git -C "$name" checkout -q --detach "$commit" || { echo "FAIL $name: commit $commit not in bundle"; exit 1; }
bun=$(command -v bun || true)
if [ -x "$HOME/.bun/bin/bun" ]; then bun=$HOME/.bun/bin/bun; fi
while read -r dir; do
  cd "$remote/$name/$dir"
  # Each validation starts cold, scoped to the repository, tree and install
  # directory, so a warm personal cache cannot conceal missing or stale inputs.
  cache="$remote/state/js-manager-caches/$name/$commit/$dir"
  mkdir -p "$cache/home"
  clean=(env)
  while IFS='=' read -r variable _; do
    upper=$(printf '%s' "$variable" | tr '[:lower:]-' '[:upper:]_')
    case "$upper" in
      BUN_INSTALL|BUN_INSTALL_CACHE_DIR|BUN_INSTALL_GLOBAL_DIR|BUN_INSTALL_BIN|XDG_CACHE_HOME|XDG_DATA_HOME|PNPM_HOME|NPM_CONFIG_CACHE|NPM_CONFIG_STORE_DIR|NPM_CONFIG_CACHE_DIR|PNPM_CONFIG_STORE_DIR|PNPM_CONFIG_CACHE_DIR)
        clean+=(-u "$variable");;
    esac
  done < <(env)
  clean+=("HOME=$cache/home" "XDG_CACHE_HOME=$cache/xdg-cache" "XDG_DATA_HOME=$cache/xdg-data" "BUN_INSTALL_CACHE_DIR=$cache/bun" "npm_config_cache=$cache/npm")
  if [ -f bun.lock ]; then lock=bun.lock; cmd=("$bun" install --frozen-lockfile --ignore-scripts --cache-dir "$cache/bun")
  elif [ -f bun.lockb ]; then lock=bun.lockb; cmd=("$bun" install --frozen-lockfile --ignore-scripts --cache-dir "$cache/bun")
  elif [ -f package-lock.json ]; then lock=package-lock.json; cmd=(npm ci --ignore-scripts --cache "$cache/npm")
  else lock=pnpm-lock.yaml; cmd=(pnpm install --frozen-lockfile --ignore-scripts --store-dir "$cache/pnpm-store" --cache-dir "$cache/xdg-cache/pnpm")
  fi
  start=$(date +%s%N)
  if out=$("${clean[@]}" "${cmd[@]}" 2>&1); then
    echo "PASS $name $dir/$lock $(( ($(date +%s%N) - start) / 1000000 )) ms"
  else
    failed=1
    echo "FAIL $name $dir/$lock: $(printf '%s\n' "$out" | grep -m1 -iE 'err|fail' | cut -c1-220)"
  fi
done
exit "$failed"
