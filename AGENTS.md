# Motor worker guide

Motor is a standalone Rust workspace. Dependencies come from crates.io and this
repository; the ordinary build and test suite needs no adjacent CortexKit
checkout and no running daemon. [CONTRIBUTING.md](CONTRIBUTING.md) is the
reference for prerequisites, the CI gate, the server gate, the operator-only
tests, and where code goes. [docs/architecture.md](docs/architecture.md) maps
the system. This guide adds what an agent working here must do.

## Running gates

- Clear inherited Git configuration for every Cargo command; some tests compare
  Git behavior exactly:

  ```sh
  env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo ...
  ```

- Run Cargo gates serially, one at a time.
- Run Linux-safe gates on the remote Linux runner when one is available. Do not
  report a macOS fallback as Linux evidence. Keep Darwin builds and tests local.
- Linux scratch needs a reflink-capable filesystem (XFS), not ext4 `/tmp`; point
  `TMPDIR` at it.
- Record `rustc --version`, `cargo --version` and `cargo nextest --version`, and
  report pass, fail and ignored counts.
- `cargo fmt --all --check` is silent on success; report its exit code and
  `cargo fmt --version`.

The portable gate, with the prefix above on each Cargo line:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --no-tests fail
python3 crates/motor-protocol/tests/generate_vectors.py --check
scripts/check-cortexkit-layout.sh
python3 scripts/test-binary-fold.py
python3 scripts/unreachable-modules.py
python3 scripts/test-dev-binaries.py
python3 scripts/test-scratch.py
python3 scripts/subc-daemon-env.py
python3 -m unittest -v scripts/tests/test_test_binary_fold.py scripts/tests/test_unreachable_modules.py scripts/tests/test_safety_fences.py
```

`python3 scripts/mixed-version.py` needs complete Git history, including the
revision pinned in `scripts/compat-client-revision`; run it on Linux.

## Release builds

```sh
# macOS module
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo build --release -p motor-module --bin ck-motor --locked
target/release/ck-motor --version
# Linux runner
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo build --release -p motor-runner --locked
target/release/motor-runner --version
```

## Operator-only tests

The ignored runner `isolated_` tests, the Windows qualification lane, and the
microVM crates' feature-gated lanes need a trusted, dedicated host with
privileges. Never run them through the remote runner or on a machine with
unrelated workloads, and never count a skip as a pass. A green portable suite
does not establish the isolation boundaries.
[CONTRIBUTING.md](CONTRIBUTING.md#tests-that-need-an-operator-host) lists them
and [docs/server-gate.md](docs/server-gate.md) runs the runner's lanes.

## Documentation

Docs live in `docs/`, one topic per file. Follow the conventions in
[docs/architecture.md](docs/architecture.md#documentation-conventions): describe
the code as it is, name the test behind each contract rule, and keep
production numbers, host names and private paths out.
