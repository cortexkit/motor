#!/usr/bin/env bash
# Motor's full pre-push gate, run on the build server as the trusted operator
# from the root of a checkout of the commit under test. It runs what CI and
# runon jobs can't: the operator-only isolation lanes (they start real
# motor-job sandboxes, so they need the operator's sudo), the real-SSH and
# openat2 tests that fail inside a sandbox, and the workspace suite twice to
# surface flaky tests. Each step prints "gate <name> ok" or "gate <name>
# FAILED", and the script exits nonzero if any step failed or the run left
# more failed motor-* systemd units than it found.
#
# Keep the checkout outside /srv/build/review: the review-copy reaper deletes
# anything there that has been idle for six hours.
set -uo pipefail

export TMPDIR=${TMPDIR:-/srv/build/tmp}
export CARGO_HOME=/srv/build/cargo-home
export RUSTUP_HOME=/home/ubuntu/.rustup
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
export PATH=/srv/build/cargo-home/bin:/home/ubuntu/.bun/bin:/usr/local/bin:/usr/bin:/bin
# The compile-cache reader tests need the cache size the runner is deployed with.
export MOTOR_READER_SCCACHE_SIZE_GIB=200
# The operator's Git config injection would change test behavior (AGENTS.md).
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0

status=0
step() {
  local name=$1
  shift
  echo "== $name"
  if "$@"; then
    echo "gate $name ok"
  else
    echo "gate $name FAILED"
    status=1
  fi
}
failed_units() {
  systemctl list-units --all --no-legend --plain 'motor-*' | awk '$4 == "failed"' | wc -l
}

before=$(failed_units)
step cortexkit-layout scripts/check-cortexkit-layout.sh
step fmt cargo fmt --all --check
step clippy cargo clippy --workspace --all-targets --locked -- -D warnings
for run in 1 2; do
  step "workspace-$run" cargo nextest run --workspace --locked --no-fail-fast
done
for run in 1 2; do
  step "operator-lib-$run" cargo nextest run -p motor-runner --lib --locked --run-ignored only \
    -E 'test(/::isolated_/) | test(=engine::tests::cancellation_deadline_and_drop_kill_even_term_ignoring_groups)' \
    --no-fail-fast
done
step operator-it cargo nextest run -p motor-runner --test it --locked --run-ignored only \
  -E 'test(serve_writes_its_tracing_events_to_stderr) | test(/::isolated_/)' --no-fail-fast
step mixed-version python3 scripts/mixed-version.py
for gate in subc-daemon-env test-binary-fold test-dev-binaries test-scratch unreachable-modules; do
  step "$gate" python3 "scripts/$gate.py"
done
after=$(failed_units)
echo "== failed motor units: before=$before after=$after"
if [ "$after" -gt "$before" ]; then
  echo "gate failed-units FAILED"
  status=1
fi
if [ "$status" = 0 ]; then echo "== gate PASSED"; else echo "== gate FAILED"; fi
exit "$status"
