# Contributing to motor

This guide covers the checks a change must pass, how to commit it, and where
code belongs. [docs/architecture.md](docs/architecture.md) explains how the
parts fit together; [AGENTS.md](AGENTS.md) adds the rules for coding agents
working in this repository.

## Prerequisites

The ordinary build and test suite needs only public tools. It does not need a
running CortexKit daemon, a credential vault, or an adjacent CortexKit checkout;
every dependency comes from crates.io or this repository.

- Rust. CI pins the toolchain in `.github/workflows/ci.yml`, with the `rustfmt`
  and `clippy` components.
- `cargo-nextest`, at the version CI pins.
- Python 3, for the protocol-vector generator and the safety fences.
- Node, npm, Bun and pnpm, for the runner's JavaScript install tests, at the
  versions `.github/workflows/ci.yml` pins.
- OpenSSH with `sshd` at `/usr/sbin/sshd`, for the real SSH transport test.
- On Linux, a scratch directory on a reflink-capable filesystem such as XFS.
  Runner fixtures clone workspaces with `FICLONE`, which ext4 does not support.
  Point `TMPDIR` at that filesystem. CI mounts a sparse XFS image for this.
- On macOS, the Cargo runner in `.cargo/config.toml` signs each test binary
  with `.cargo/sign-test-binary.sh` before it runs.

Some tests compare Git behavior exactly. If your shell injects Git configuration
through `GIT_CONFIG_COUNT`, clear it for every Cargo command:

```sh
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo ...
```

The commands below omit that prefix for readability.

## The gate

Three gates exist. Each covers what the one before it cannot.

### Hosted CI

`.github/workflows/ci.yml` runs on every push and pull request, on hosted Linux:

```sh
scripts/check-cortexkit-layout.sh
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --no-tests fail
python3 scripts/mixed-version.py
python3 crates/motor-protocol/tests/generate_vectors.py --check
python3 scripts/test-binary-fold.py
python3 scripts/unreachable-modules.py
python3 scripts/test-dev-binaries.py
python3 scripts/test-scratch.py
python3 scripts/subc-daemon-env.py
python3 scripts/run-script-tests.py
```

What each check guards:

| Check | Guards |
| --- | --- |
| `check-cortexkit-layout.sh` | Nothing under `.cortexkit/` is tracked except the allowed folders. |
| `cargo nextest run --no-tests fail` | The portable suite; fails if no test was selected. |
| `mixed-version.py` | Wire compatibility in both directions with the client revision pinned in `scripts/compat-client-revision`. Needs full Git history. |
| `generate_vectors.py --check` | The [protocol corpus](test-vectors/exec-remote-v1/README.md) matches its generator byte for byte. |
| `test-binary-fold.py` | Each crate keeps one integration test binary (`tests/it/`) unless `scripts/test-binary-fold.allow` names a reason. |
| `unreachable-modules.py` | Every library module is reachable from a binary, or `scripts/unreachable-modules.allow` says why not. |
| `test-dev-binaries.py` | Tests never launch production `ck-*` executable names. |
| `test-scratch.py` | Rust tests never use global temporary-directory constructors such as `std::env::temp_dir()`; they use `cortexkit_test_support::scratch_root()`, which keeps scratch inside a per-test directory. |
| `subc-daemon-env.py` | Tests never locate or spawn the CortexKit daemon except through `cortexkit_test_support::SubcDaemonBinary`, which points all of the daemon's paths into the test's scratch directory. |
| `run-script-tests.py` | The Python tests of the fences above (they reject planted violations, so a fence cannot pass by seeing nothing) and of the operator scripts under `script/`. Some need Linux tools such as `flock`; CI runs them on Linux. |

### The server gate

`script/runner/server-gate.sh` is the full pre-push gate. It runs on a dedicated
Linux build server as the trusted operator, because it starts real `motor-job`
sandboxes. It adds the operator-only lanes CI cannot run; `GATE_RUNS=2` repeats
the suites when hunting a flaky test. [docs/server-gate.md](docs/server-gate.md)
lists its steps and how to read its result.

### Tests that need an operator host

These tests are `#[ignore]` or behind a Cargo feature in the ordinary suite. A
passing portable suite does not establish the boundaries they check.

| Tests | Needs | Where they run |
| --- | --- | --- |
| `motor-runner` tests whose names contain `::isolated_` | `motor-job` and `motor-cache` accounts, systemd with private PID support, ACL tools, reflink storage, delegated sudo | Server gate |
| `engine::tests::cancellation_deadline_and_drop_kill_even_term_ignoring_groups` | Real process groups on the build server | Server gate |
| `serve_writes_its_tracing_events_to_stderr` (`motor-runner`, `tests/it`) | A real `serve` process | Server gate |
| `motor-vm-helper --features linux-root-tests` and `linux-service-tests` | Root, systemd 254 or newer, cgroup v2, the Firecracker jailer | By hand; see [docs/linux-microvm.md](docs/linux-microvm.md) |
| `motor-vm-guest --features linux-root-tests` and `trusted-cargo-tests` | Root and namespaces; network access and a trusted Cargo | By hand; see [docs/linux-microvm.md](docs/linux-microvm.md) |
| `motor-vm-loader --features linux-uffd-tests` and `linux-root-tests` | `userfaultfd` permission; root and a static loader binary | By hand; see [docs/linux-microvm.md](docs/linux-microvm.md) |
| `motor-module` `motor_e2e` binary (macOS only) | A real runner reachable over SSH; its inputs are the `CK_MOTOR_E2E_*` variables named at the top of `crates/motor-module/tests/motor_e2e.rs` | By hand on a macOS client, with the command in that file's header |
| Real macOS daemon and vault integration | An installed CortexKit daemon and credential vault | Not in this repository |

Operator lanes fail on a missing prerequisite instead of counting a skip as a
pass.

The scripts under `script/` carry their own Python tests next to them. CI and the
server gate run them all through `scripts/run-script-tests.py`; a new test file
is added to the list in that script.

## Committing

- Keep each commit one coherent change that passes the CI gate on its own.
- Describe the change in the subject line. Explain in the body why the change
  is needed when the diff does not make that obvious.
- If a change alters an existing test's expectation, say why in the commit
  message. A test states that a property matters; changing it silently removes
  that claim.
- Never force-add files under `.cortexkit/`; the layout check fails on them.
- Keep secrets, `.env` files, logs and `target/` out of commits. `.gitignore`
  covers the usual cases.
- If you change the wire protocol, regenerate the corpus with
  `python3 crates/motor-protocol/tests/generate_vectors.py` and commit the
  result together with the code.

## Where code goes

| Change | Place |
| --- | --- |
| Wire frames and codec shared by client and runner | `crates/motor-protocol` |
| Caller-facing request and outcome types | The published `cortexkit-exec-remote-types` crate; `motor-protocol` re-exports them |
| Snapshot capture, transport, credentials, caller admission | `crates/motor-module` |
| Admission, queueing, transfers, workspaces, retention | `crates/motor-runner/src/engine.rs`, `server.rs`, `workspace/` |
| Linux job sandbox | `crates/motor-runner/src/isolation*`, `systemd.rs`, `network*.rs` |
| Windows VM backend, host side | `crates/motor-runner/src/windows/` |
| Windows guest agent | `crates/motor-winagent`, a separate crate with its own `Cargo.lock`, excluded from the workspace |
| Compile cache and warm builds | `crates/motor-runner/src/compile_cache.rs`, `writer/` |
| Linux microVM components | `crates/motor-vm-helper`, `motor-vm-guest`, `motor-vm-loader` |
| Build server scripts and units | `script/runner/` |
| Windows image and VM scripts | `script/windows-vm/` |
| Repository gates and fences | `scripts/` |
| Documentation | `docs/`, one topic per file |

Tests follow these rules, which the checks in `scripts/` enforce:

- Integration tests live as modules under the crate's `tests/it/`, not as new
  top-level test files.
- Test scratch directories come from `cortexkit_test_support::scratch_root()`.
- A test that needs a CortexKit daemon gets it from
  `cortexkit_test_support::SubcDaemonBinary`.
- A test that needs a privileged host is `#[ignore]` (runner) or behind a
  Cargo feature (VM crates), and fails rather than skips when a prerequisite is
  missing.

Documentation follows the conventions in [docs/architecture.md](docs/architecture.md#documentation-conventions).

## Reporting security issues

Do not open a public issue for a vulnerability. [SECURITY.md](SECURITY.md)
explains how to report one privately.
