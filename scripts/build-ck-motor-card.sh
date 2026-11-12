#!/usr/bin/env bash
# Build a mainline card binary; signing and staging are deliberately separate.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

git fetch origin
MOTOR_BUILD_COMMIT=$(git rev-parse HEAD)
status=$(git status --porcelain)
if [[ -n "$status" ]]; then
  echo "Refusing to build a card from a dirty tree." >&2
  exit 1
fi
if ! git merge-base --is-ancestor "$MOTOR_BUILD_COMMIT" origin/main; then
  echo "Refusing to build a card: HEAD is not on origin/main." >&2
  exit 1
fi
tip=$(git rev-parse origin/main)
if [[ "$MOTOR_BUILD_COMMIT" == "$tip" ]]; then
  echo "Building $MOTOR_BUILD_COMMIT (origin/main tip)"
else
  echo "Building $MOTOR_BUILD_COMMIT (ancestor of origin/main; tip is $tip)"
fi

sha256() {
  python3 -c 'import hashlib, pathlib, sys; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())' "$1"
}

export MOTOR_BUILD_COMMIT
export CK_BUILD_GIT_SHA="$MOTOR_BUILD_COMMIT"
export CK_BUILD_GIT_STATUS=clean
CK_BUILD_LOCK_DIGEST=$(sha256 "$ROOT/Cargo.lock")
export CK_BUILD_LOCK_DIGEST
# Read rustc's whole output: a reader that stops at the host line can make rustc
# fail writing the lines after it, and pipefail then ends this script silently.
host=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$host" ]] || { echo "rustc -vV reported no host" >&2; exit 1; }
target_dir=$(cargo metadata --no-deps --format-version=1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
cargo build --locked --release --target "$host" -p motor-module --bin ck-motor
binary="$target_dir/$host/release/ck-motor"
if [[ "$host" == *windows* ]]; then
  binary+=.exe
fi
binary_digest=$(sha256 "$binary")
printf 'Binary: %s\nSHA256: %s\n' "$binary" "$binary_digest"
