# Running the server gate

`script/runner/server-gate.sh` is motor's full pre-push gate. It runs what
hosted CI and remote build jobs cannot: the operator-only isolation tests, which
start real `motor-job` sandboxes, and the tests that need a real SSH server or
`openat2` outside a sandbox. Each suite runs once; set `GATE_RUNS=2` or more to
repeat the workspace and operator suites when hunting a flaky test.
[CONTRIBUTING.md](../CONTRIBUTING.md#the-gate) explains how it
relates to hosted CI.

## Before you start

- A dedicated Linux build server installed as in
  [runner-install.md](runner-install.md): the `motor-job` and `motor-cache`
  accounts, systemd with private PID support, ACL tools, and reflink-capable
  storage under `/srv/build`.
- The operator account that holds the runner's delegated `sudo` rules. The
  isolation tests start transient system services, so they cannot run as an
  unprivileged user or inside a build job.
- A checkout of the commit under test, outside `/srv/build/review`. The review
  reaper (`script/runner/review_reaper.py`) deletes anything there that has been
  idle for six hours.
- No unrelated workloads on the machine. The isolation tests create and stop
  real system units.

The script sets its own environment: `TMPDIR` (default `/srv/build/tmp`),
`CARGO_HOME`, `RUSTUP_HOME`, `PATH`, and `CARGO_TARGET_DIR` (default `target/`
in the checkout). It clears `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0` and
`GIT_CONFIG_VALUE_0`. It sets `MOTOR_READER_SCCACHE_SIZE_GIB` to the compile
cache size the runner is deployed with, because the cache-reader tests compare
against it. The paths are fixed in the script; read its first lines before
running it on a host laid out differently.

Before privileged isolation fixtures, the gate builds the ordinary runner binary
and runs `check-permissions-helper`. An administrator must first install the
matching root-owned helper and narrow sudo fragment with
`script/runner/install-permissions-helper.sh`. Test harness executables are never
run under sudo as helpers. The fixed helper path, ownership/modes, code digest
and both delegated command forms are checked before the operator lanes.

## Run it

From the root of the checkout:

```sh
script/runner/server-gate.sh
```

To check only the installed memory policy, after installing units with
`script/runner/install-units.sh`, draining all instances, rebooting, and starting
the runner user service, run as the operator on a dedicated host (not through a
remote build job):

```sh
MOTOR_TEST_RUNNER_UNIT=motor-runner.service TMPDIR=/srv/build/tmp \
  env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  cargo nextest run -p motor-runner --lib --locked --run-ignored only \
  -E 'test(=isolation::tests::isolated_job_memory_budget_and_oom_priority_protect_runner)'
```

Use `motor-runner@windows.service` to inspect the Windows instance instead.
The test requires delegated sudo for real job units and the matching installed
permission helper. It reads the live runner's OOM score, not the test harness's.

## What it runs

Each step prints `== <name>`, then `gate <name> ok` or `gate <name> FAILED`.
A failed step does not stop later steps.

| Step | Command |
| --- | --- |
| `cortexkit-layout` | `scripts/check-cortexkit-layout.sh` |
| `fmt` | `cargo fmt --all --check` |
| `clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| `workspace-1` (up to `workspace-N` with `GATE_RUNS=N`) | `cargo nextest run --workspace --locked --no-fail-fast` |
| `operator-lib-1` (up to `operator-lib-N`) | `cargo nextest run -p motor-runner --lib --locked --run-ignored only` with the filter `test(/::isolated_/) \| test(=engine::tests::cancellation_deadline_and_drop_kill_even_term_ignoring_groups)` |
| `operator-it` | `cargo nextest run -p motor-runner --test it --locked --run-ignored only` with the filter `test(serve_writes_its_tracing_events_to_stderr) \| test(/::isolated_/)` |
| `mixed-version` | `python3 scripts/mixed-version.py` |
| `subc-daemon-env`, `test-binary-fold`, `test-dev-binaries`, `test-scratch`, `unreachable-modules` | `python3 scripts/<name>.py` |
| `script-tests` | `python3 scripts/run-script-tests.py`: the Python tests of those fences and of the operator scripts under `script/` |

Before the first step and after the last, the script counts systemd units
matching `motor-*` in the `failed` state.

## Read the result

The last line is `== gate PASSED` or `== gate FAILED`, and the exit status is
0 or 1 to match. The gate fails when any step failed, or when the run left more
failed `motor-*` units than it found. A new failed unit means a test leaked a
sandbox service; inspect it with `systemctl status <unit>` and clear it with
`systemctl reset-failed <unit>` once understood.

The gate does not cover the Linux microVM crates' root and service features,
the Windows VM backend against a real Windows image, or the macOS end-to-end
binary. [CONTRIBUTING.md](../CONTRIBUTING.md#tests-that-need-an-operator-host)
lists where those run.

## Undo

The gate changes nothing outside its checkout and `CARGO_TARGET_DIR`, apart
from the transient units it starts and stops. To reclaim space, remove the
checkout's `target/` directory.
