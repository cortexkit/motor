#!/usr/bin/env bash
# Run CI's ordinary gate before pushing. Fast failures never submit remote jobs.
#
# Parity with .github/workflows/ci.yml (all push jobs, including push to main):
# CI job / step                                      -> gate lane
# linux-ordinary / checkout (full history)             -> source snapshots
#   Rust 1.99.0 + rustfmt/clippy, nextest 0.9.138       -> provisioned runner tools
#   Node 24.21.0, Bun 1.4.2, pnpm 11.1.2, JS layout   -> linux-tests (runner tools)
#   Tool versions                                    -> linux-lint, linux-tests
#   Nothing tracked outside .cortexkit allowlist      -> fast
#   Formatting                                       -> fast
#   All-target workspace check, Clippy                -> linux-lint
#   Windows runner Clippy (cross-check, not Windows)  -> windows-clippy (Linux)
#   Prepare SSH fixture + XFS reflink scratch         -> linux-tests (runner setup)
#   Workspace tests, pinned client compatibility     -> linux-tests
#   Protocol vectors                                 -> fast
# linux-musl-runner / checkout, Rust, musl compiler    -> musl (runner tools)
#   Build static tenant runner                       -> musl
# python-safety / checkout, Rust, Python 3.11+         -> snapshots / runner tools
#   binary-fold, unreachable, dev-binaries, scratch,
#   subc-daemon-env                                   -> fast
#   run-script-tests.py                              -> script-tests
#     runner/Boat/tenant source pin freshness subset  -> fast (also script-tests)
# ci-success / All jobs succeeded                     -> final exit status
#
# GitHub actions (checkout, tool installs/cache, sudo SSH/XFS setup) are not
# replayed: direct jobs have no host-admin privileges and use the build server's
# provisioned tools, sshd and reflink scratch instead. The lanes check their
# prerequisites and fail rather than silently omit checks. No hosted-only test
# step is omitted. Operator-only isolation/VM tests are not CI push jobs.
# Extra pre-push checks: fast lockfile/unmerged checks; native macOS clippy.
# Cargo runs serially within each lane; persistent, separate checkouts let lanes
# run in parallel without sharing a remote workspace or losing warm build output.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
root=$(pwd -P)

usage() {
  echo 'Usage: scripts/gate.sh [--fast | --lane NAME ...] [--serial]'
  echo 'Default: fast, then all available lanes in parallel.'
  echo 'Lanes: linux-lint linux-tests windows-clippy musl script-tests macos-clippy'
  echo '--lane always runs fast first; --fast submits no jobs and builds nothing.'
  echo '--serial runs remote lanes in one job from this checkout (no side clones).'
  echo 'Parallel checkouts: $MOTOR_GATE_ROOT or ~/Work/Projects/CortexKit-scratch/motor-gate/<lane>.'
  echo 'Requires Python 3.11+, Cargo and (for remote lanes) ck motor run.'
}

lanes=()
fast_only=false
serial=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --fast) fast_only=true; shift ;;
    --serial) serial=true; shift ;;
    --lane)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      case "$2" in
        linux-lint|linux-tests|windows-clippy|musl|script-tests|macos-clippy) ;;
        *) echo "Unknown lane: $2" >&2; exit 2 ;;
      esac
      for lane in "${lanes[@]+${lanes[@]}}"; do
        [ "$lane" != "$2" ] || { echo "Duplicate lane: $2" >&2; exit 2; }
      done
      lanes+=("$2"); shift 2 ;;
    *) usage >&2; exit 2 ;;
  esac
done
if $fast_only && [ "${#lanes[@]}" -ne 0 ]; then
  echo '--fast cannot be combined with --lane' >&2
  exit 2
fi
if [ "${#lanes[@]}" -eq 0 ] && ! $fast_only; then
  lanes=(linux-lint linux-tests windows-clippy musl script-tests)
  if [ "$(uname -s)" = Darwin ] && command -v cargo >/dev/null; then
    lanes+=(macos-clippy)
  fi
fi

# macOS's system Python is too old for tomllib in the safety checks.
python=${PYTHON:-}
if [ -z "$python" ]; then
  for candidate in python3 python3.14 python3.13 python3.12 python3.11; do
    if command -v "$candidate" >/dev/null && "$candidate" -c 'import tomllib' 2>/dev/null; then
      python=$candidate
      break
    fi
  done
fi
[ -n "$python" ] || { echo 'Python 3.11+ is required (or set PYTHON)' >&2; exit 2; }

unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
export CARGO_TERM_COLOR=never
fast() {
  local unmerged
  unmerged=$(git diff --name-only --diff-filter=U) || return
  if [ -n "$unmerged" ]; then
    printf 'Unresolved merge files:\n%s\n' "$unmerged" >&2
    return 1
  fi
  cargo fmt --version || return
  cargo fmt --all --check || return
  # Resolve the entire graph: --no-deps can miss a stale lockfile.
  cargo metadata --locked --offline --format-version 1 >/dev/null || return
  bash scripts/check-cortexkit-layout.sh || return
  "$python" crates/motor-protocol/tests/generate_vectors.py --check || return
  # Source digests and manifests must be fresh before spending time compiling.
  "$python" script/runner/test_toolchain_parity.py \
    ParityTests.test_host_file_contract_requires_valid_pins_and_metadata \
    ParityTests.test_trusted_compiler_wrapper_hash_matches_runner_source || return
  "$python" script/boat-image/test_boat_image.py RecipeTests.test_pins_match_real_runner_inputs || return
  "$python" script/tenant-image/test_tenant_image.py ImageTests.test_pins_cover_real_runner_inputs || return
  local check
  for check in test-binary-fold unreachable-modules test-dev-binaries test-scratch subc-daemon-env; do
    "$python" "scripts/$check.py" || return
  done
}

start=$SECONDS
if fast; then
  echo "gate fast PASS ($((SECONDS - start))s)"
else
  code=$?
  echo "gate fast FAIL exit=$code ($((SECONDS - start))s)"
  exit "$code"
fi
$fast_only && exit 0

command_for() {
  case "$1" in
    linux-lint) printf '%s\n' '
      rustc --version
      cargo --version
      cargo check --workspace --all-targets --locked
      cargo clippy --workspace --all-targets --locked -- -D warnings' ;;
    linux-tests) printf '%s\n' '
      node --version; npm --version; bun --version; pnpm --version
      /usr/local/bin/node --version
      /usr/local/bin/npm --version
      /usr/local/bin/pnpm --version
      rustc --version; cargo --version; cargo fmt --version
      cargo nextest --version; python3 --version
      test -x /usr/sbin/sshd
      test -n "${TMPDIR:-}" && test -d "$TMPDIR"
      scratch=$(mktemp -d "$TMPDIR/motor-gate.XXXXXX")
      trap '\''rm -rf "$scratch"'\'' EXIT
      printf reflink >"$scratch/source"
      cp --reflink=always "$scratch/source" "$scratch/copy"
      cargo nextest run --workspace --locked --no-tests fail
      python3 scripts/mixed-version.py' ;;
    windows-clippy) printf '%s\n' '
      rustup target add x86_64-pc-windows-gnu
      cargo clippy -p motor-runner --target x86_64-pc-windows-gnu --lib --bins --locked -- -D warnings' ;;
    musl) printf '%s\n' '
      command -v musl-gcc >/dev/null || { echo "musl-gcc is required; install musl-tools" >&2; exit 1; }
      rustup target add x86_64-unknown-linux-musl
      export CC_x86_64_unknown_linux_musl=musl-gcc
      cargo build --locked --target x86_64-unknown-linux-musl -p motor-runner' ;;
    script-tests) printf '%s\n' '
      python3 --version
      python3 scripts/run-script-tests.py' ;;
    macos-clippy) printf '%s\n' '
      test "$(uname -s)" = Darwin
      cargo clippy -p motor-runner -p motor-macvm -p motor-macagent -p motor-module -p motor-vm-helper --all-targets --locked -- -D warnings' ;;
  esac
}

mkdir -p .cortexkit/internal-docs/gate
logs=$(mktemp -d "$root/.cortexkit/internal-docs/gate/run.XXXXXX")
pids=()
locked=false
cleanup() {
  local pid
  for pid in "${pids[@]+${pids[@]}}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]+${pids[@]}}"; do wait "$pid" 2>/dev/null || true; done
  if $locked; then
    rm -f "$lane_root/.lock/pid"
    rmdir "$lane_root/.lock"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

remote_lanes=()
native=false
for lane in "${lanes[@]}"; do
  if [ "$lane" = macos-clippy ]; then native=true; else remote_lanes+=("$lane"); fi
done
if [ "${#remote_lanes[@]}" -gt 0 ]; then
  command -v ck >/dev/null || { echo 'ck motor run is required' >&2; exit 2; }
fi
if [ "${#remote_lanes[@]}" -gt 0 ] && ! $serial; then
  # Stable sibling roots preserve each remote workspace's warm Cargo output.
  # Nested roots are refused by the runner, even if they have separate .git dirs.
  lane_root=${MOTOR_GATE_ROOT:-"$HOME/Work/Projects/CortexKit-scratch/motor-gate"}
  mkdir -p "$lane_root"
  lane_root=$(cd "$lane_root" && pwd -P)
  case "$lane_root/" in
    "$root/"*) echo 'Lane checkouts must be outside the caller checkout' >&2; exit 2 ;;
  esac
  if ! mkdir "$lane_root/.lock" 2>/dev/null; then
    echo "Another gate owns $lane_root/.lock; wait for it, or remove a stale lock after checking its pid" >&2
    exit 2
  fi
  locked=true
  echo "$$" >"$lane_root/.lock/pid"
  # An alternate index captures staged AND unstaged source without touching the
  # caller's index, branch or files. A temporary commit carries complete history
  # for mixed-version.py and makes resets remove stale source from earlier runs.
  source_index=$(git rev-parse --git-path index)
  snapshot=$(
    set -e
    export GIT_INDEX_FILE="$logs/source.index"
    cp "$source_index" "$GIT_INDEX_FILE"
    git add -A -- .
    git rm -r --cached --quiet --ignore-unmatch -- target
    tree=$(git write-tree)
    printf '%s\n' 'Gate source snapshot' | git -c user.name='Motor gate' -c user.email='gate@localhost' commit-tree "$tree" -p HEAD
  )
  for lane in "${remote_lanes[@]}"; do
    checkout="$lane_root/$lane"
    if [ ! -e "$checkout" ]; then
      git clone --quiet --no-checkout "$root" "$checkout"
      echo 'motor-gate' >"$checkout/.git/motor-gate-owned"
    fi
    if [ ! -f "$checkout/.git/motor-gate-owned" ] || [ "$(<"$checkout/.git/motor-gate-owned")" != motor-gate ]; then
      echo "Refusing to reset a checkout not created by this gate: $checkout" >&2
      exit 2
    fi
    git -C "$checkout" fetch --quiet --no-tags "$root" "$snapshot"
    git -C "$checkout" reset --quiet --hard FETCH_HEAD
  done
fi

body_for() {
  printf '%s\n' 'set -euo pipefail
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
export CARGO_TERM_COLOR=never'
  command_for "$1"
}

if $serial && [ "${#remote_lanes[@]}" -gt 0 ]; then
  serial_body='set -uo pipefail; failed=0'
  for lane in "${remote_lanes[@]}"; do
    printf -v quoted '%q' "$(body_for "$lane")"
    serial_body+="
start=\$SECONDS; code=0
bash -c $quoted || code=\$?
if [ \"\$code\" -eq 0 ]; then result=PASS; else result=FAIL; failed=1; fi
echo \"gate $lane \$result exit=\$code (\$((SECONDS - start))s)\""
  done
  serial_body+=$'\nexit "$failed"'
  lanes=(remote-serial)
  # Native macOS lint cannot run inside the Linux serial job.
  $native && lanes+=(macos-clippy)
fi

echo "gate logs: $logs"
for lane in "${lanes[@]}"; do
  (
    start=$SECONDS
    code=0
    body=$(body_for "$lane")
    if [ "$lane" = macos-clippy ]; then
      bash -c "$body" >"$logs/$lane.log" 2>&1 || code=$?
    elif [ "$lane" = remote-serial ]; then
      ck motor run --vcpus 16 --cwd "$root" -- "$serial_body" >"$logs/$lane.log" 2>&1 || code=$?
    else
      ck motor run --vcpus 16 --cwd "$lane_root/$lane" -- "$body" >"$logs/$lane.log" 2>&1 || code=$?
    fi
    echo "$code $((SECONDS - start))" >"$logs/$lane.result"
  ) &
  pids+=("$!")
done

failed=0
for index in "${!lanes[@]}"; do
  lane=${lanes[$index]}
  wait "${pids[$index]}" || true
  if [ "$lane" = remote-serial ]; then
    grep '^gate ' "$logs/$lane.log" || true
  fi
  if [ -f "$logs/$lane.result" ]; then
    read -r code elapsed <"$logs/$lane.result"
  else
    code=1; elapsed=unknown
  fi
  if [ "$code" -eq 0 ]; then
    echo "gate $lane PASS (${elapsed}s)"
  else
    echo "gate $lane FAIL exit=$code (${elapsed}s); log: $logs/$lane.log"
    failed=1
  fi
done
pids=()
exit "$failed"
