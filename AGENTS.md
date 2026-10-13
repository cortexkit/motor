# Motor worker guide

Motor is a standalone Rust workspace. Use dependencies from crates.io and this
repository; no adjacent CortexKit checkout or running daemon is needed for the
ordinary build and test suite.

## Portable gates

Run from the repository root. Clear inherited Git configuration for every Cargo
command with the prefix below. Run Cargo gates serially. For Linux-safe gates,
use the remote Linux runner when available; do not report a macOS fallback as
Linux evidence. Keep Darwin builds and tests local.

Record `rustc --version`, `cargo --version`, and `cargo nextest --version`, and
report pass/fail/ignored counts. Prerequisites are Rust, cargo-nextest, and Python 3.
`cargo fmt --all --check` is silent on success; report its exit code and
`cargo fmt --version`.

```sh
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo fmt --all --check
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo metadata --locked --format-version 1
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo check --workspace --all-targets --locked
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run --workspace --locked
python3 crates/motor-protocol/tests/generate_vectors.py --check
```

Run the repository safety fences and their controls:

```sh
python3 scripts/test-binary-fold.py
python3 scripts/unreachable-modules.py
python3 scripts/test-dev-binaries.py
python3 scripts/test-scratch.py
python3 scripts/subc-daemon-env.py
python3 -m unittest -v scripts/tests/test_test_binary_fold.py scripts/tests/test_unreachable_modules.py scripts/tests/test_safety_fences.py
```

The dev-binary gate includes its own unittest suite; the scratch gate runs built-in
constructor controls. The separate self-tests plant violations in scratch trees.
Linux scratch needs a native reflink-capable XFS filesystem, not ext4 `/tmp`.
The real SSH transport fixture needs OpenSSH and sshd. On macOS, the Cargo runner
in `.cargo/config.toml` signs test binaries.

## Release builds

```sh
# macOS module
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo build --release -p motor-module --bin ck-motor --locked
target/release/ck-motor --version
# Linux runner
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo build --release -p motor-runner --locked
target/release/motor-runner --version
```

## Hosted CI and compatibility

Hosted CI runs ordinary Linux workspace checks/tests, protocol vectors, safety
fences, and the mixed-version compatibility gate. It does not provision privileged
runner isolation, root-only helper tests, or real macOS integration tests.

Run `scripts/mixed-version.py`, the mixed-version gate, separately on Linux:

```sh
python3 scripts/mixed-version.py
```

It needs complete Git history, including the full revision pinned in
`scripts/compat-client-revision`. It builds older client and runner helpers and
tests both version directions over real byte streams. A current direct client
must omit gate marks when the older runner cannot negotiate them. These tests are
ignored because the script supplies the helpers. The dedicated gate selects them
explicitly and fails if no test runs or the pinned Git object is missing.

## Trusted operator gates

The ignored runner `isolated_` tests belong on a trusted, dedicated Linux host,
not hosted CI or a machine holding unrelated workloads. They require native
same-filesystem reflink storage, `motor-job` for isolated commands and the distinct
non-root `motor-cache` account for trusted cache fills,
systemd private-PID support, ACL tools, and narrowly scoped privilege delegation
specified in the [runner guide](crates/motor-runner/README.md). Provision those
prerequisites for the test installation; do not infer them from the portable suite.

The [VM helper runbook](crates/motor-vm-helper/RUNBOOK.md) and
[guest guide](crates/motor-vm-guest/README.md) document separate opt-in Linux
root/service tests and what requires real VMs. Privileged lanes must fail on
missing prerequisites rather than count a skip as acceptance. Real macOS daemon
and vault integration also requires an external CortexKit installation and is
not exercised by the standalone portable suite.
