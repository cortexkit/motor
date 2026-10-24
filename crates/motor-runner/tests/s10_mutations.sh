#!/bin/bash
# Mutation driver for the direct-runner memory slice: each case mutates one
# path, prints the mutated line, then runs only what should notice. The remote
# copy is thrown away, so the checkout never holds a mutation.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 2

run() {
    local name=$1 path=$2 python_mut=$3 test_filter=$4
    echo "=== CASE $name"
    python3 - "$path" <<PY
import sys
path = sys.argv[1]
source = open(path).read()
$python_mut
open(path, "w").write(source)
for i, line in enumerate(source.splitlines(), 1):
    if "MOTOR_MUTATION" in line:
        print(f"MUTATED {path}:{i}: {line.strip()}")
PY
    git diff --numstat -- "$path"
    cargo test -p motor-runner --lib -- "$test_filter" 2>&1 | grep -E "^(test |error\[|error:|test result:)"
    git checkout -- "$path"
}

run clamp src/isolation.rs '
old = ".map_or(memory_max, |budget| memory_max.min(budget.max));"
assert old in source
source = source.replace(old, ".map_or(memory_max, |budget| memory_max.max(budget.max)); // MOTOR_MUTATION")
' job_memory_limit_never_exceeds_the_slice_memory_max

run refusal src/engine.rs '
old = """        } else if let Some(detail) = self.slice_memory_refusal() {
            Some((RefusalReason::RunnerFull, detail))"""
assert old in source
source = source.replace(old, """        } else if let Some(detail) = self.slice_memory_refusal() {
            let _ = (detail, RefusalReason::RunnerFull); // MOTOR_MUTATION
            None""")
' slice_below_the_job_floor_is_refused_and_a_clamp_is_not

run universe src/backend.rs '
old = "    (floor > budget.max).then(|| {"
assert old in source
source = source.replace(old, "    (floor > budget.max && false).then(|| { // MOTOR_MUTATION")
' slice_below_the_job_floor_is_refused_and_a_clamp_is_not
