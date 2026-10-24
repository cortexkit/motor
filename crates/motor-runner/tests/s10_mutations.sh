#!/bin/bash
# Mutation driver for the direct-runner memory slice. Each case mutates one
# path, prints the mutated line, then runs only what should notice. The remote
# copy is thrown away, so the checkout never holds a mutation.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2

mutate() {
    case "$1" in
    clamp)
        perl -0pi -e 's/\.map_or\(memory_max, \|budget\| memory_max\.min\(budget\.max\)\);/\.map_or(memory_max, |budget| memory_max.max(budget.max)); \/\/ MOTOR_MUTATION/' "$2"
        ;;
    admission)
        perl -0pi -e 's/(\} else if let Some\(detail\) = self\.slice_memory_refusal\(\) \{\n            Some\(\(RefusalReason::RunnerFull, detail\)\)\n)/} else if let Some(detail) = self.slice_memory_refusal() {\n            let _ = (detail, RefusalReason::RunnerFull); \/\/ MOTOR_MUTATION\n            None\n/' "$2"
        ;;
    rule)
        perl -0pi -e 's/    \(floor > budget\.max\)\.then\(\|\| \{/    (floor > budget.max \&\& false).then(|| { \/\/ MOTOR_MUTATION/' "$2"
        ;;
    esac
}

run() {
    local name=$1 path=$2 test_filter=$3
    echo "=== CASE $name"
    mutate "$name" "$path"
    if ! grep -q MOTOR_MUTATION "$path"; then
        echo "MUTATION NOT APPLIED"
    fi
    grep -n "MOTOR_MUTATION" "$path"
    git diff --numstat -- "$path" | cat
    cargo test -p motor-runner --lib -- "$test_filter" 2>&1 | grep -E "^(test |error\[|error:|test result:)"
    echo "cargo exit ${PIPESTATUS[0]}"
    git checkout -- "$path" 2>/dev/null | cat
    perl -0pi -e 's/ \/\/ MOTOR_MUTATION//' "$path" 2>/dev/null
    grep -c MOTOR_MUTATION "$path"
}

run clamp src/isolation.rs job_memory_limit_never_exceeds_the_slice_memory_max
run admission src/engine.rs jobs_slice_below_the_job_floor_refuses_admission_before_start
run rule src/backend.rs slice_below_the_job_floor_is_refused_and_a_clamp_is_not
