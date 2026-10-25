#!/bin/bash
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
echo "=== mutant: drop the per-admission slice read"
perl -0pi -e 's/        self\.refresh_slice_budget\(\);\n        let \(_admission, initial_refusal\) = \{/        \/\/ MOTOR_MUTATION\n        let (_admission, initial_refusal) = {/' src/engine.rs
grep -n "MOTOR_MUTATION" src/engine.rs
git diff --numstat -- src/engine.rs | cat
cargo test -p motor-runner --lib -- jobs_slice 2>&1 | grep -E "^(test |error\[|error:|test result)"
echo "cargo exit ${PIPESTATUS[0]}"
perl -0pi -e 's/        \/\/ MOTOR_MUTATION\n//' src/engine.rs
git checkout -- src/engine.rs 2>/dev/null | cat
