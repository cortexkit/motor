#!/bin/bash
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2
echo "=== mutant: read the slice while holding the state lock"
perl -0pi -e 's/        self\.refresh_slice_budget\(\);\n        let \(_admission, initial_refusal\) = \{/        { let _held = self.core.inner.lock().unwrap(); self.refresh_slice_budget(); } \/\/ MOTOR_MUTATION\n        let (_admission, initial_refusal) = {/' src/engine.rs
grep -n "MOTOR_MUTATION" src/engine.rs
git diff --numstat -- src/engine.rs | cat
cargo test -p motor-runner --lib -- the_admission_slice_read_happens_before_the_state_lock 2>&1 | grep -E "^(test |error|test result)"
echo "cargo exit ${PIPESTATUS[0]}"
perl -0pi -e 's/ \/\/ MOTOR_MUTATION//' src/engine.rs
git checkout -- src/engine.rs 2>/dev/null | cat
