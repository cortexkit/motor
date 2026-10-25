#!/bin/bash
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
echo "=== mutant: maintenance no longer re-reads the slice"
perl -0pi -e 's/        \/\/ Outside the state lock, which maintenance later takes\.\n        self\.refresh_slice_budget\(\);/        \/\/ MOTOR_MUTATION/' src/engine.rs
grep -n "MOTOR_MUTATION" src/engine.rs
git diff --numstat -- src/engine.rs | cat
cargo test -p motor-runner --lib -- jobs_slice 2>&1 | grep -E "^(test |error\[|error:|test result)"
echo "cargo exit ${PIPESTATUS[0]}"
perl -0pi -e 's/        \/\/ MOTOR_MUTATION/        \/\/ Outside the state lock, which maintenance later takes.\n        self.refresh_slice_budget();/' src/engine.rs
git checkout -- src/engine.rs 2>/dev/null | cat
