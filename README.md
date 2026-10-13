# Motor

Motor is a Rust workspace for remote command execution. `ck-motor` is the client
module: it snapshots workspaces and sends them over SSH to `motor-runner` on
Linux. The runner owns Git mirrors, admission and queueing, isolated execution,
and retained output for attach/replay. The workspace also contains the Linux VM
helper, guest supervisor and loader, and a vendored build-slot supervisor.

## Integration boundary

`ck-motor` provides `exec-remote/v1` as a reserved module for the CortexKit daemon
(SubC). SubC verifies caller identities. AFT, the agent-tool caller, may request
`exec.run`, `exec.attach`, `exec.cancel`, and `exec.status`. The CortexKit core
(`reserved:prefrontal-core`) prepares and drops workspaces. Admission is per
operation, not a blanket grant from a management-surface bind. Basal's authorized
health flow can read health; Claustrum supplies public-key lookup and vault-only
SSH signing. SubC and these integrations do not ship in this repository.

`motor-protocol` re-exports the public caller types from
`cortexkit-exec-remote-types`. Its framed version-1 client-runner protocol is
internal to motor, distinct from that public API: it carries snapshot bundles,
queue control, and runner streams between `ck-motor` and `motor-runner`. Here,
internal means an implementation boundary, not a secret or unpublished protocol.
The [synthetic protocol corpus](test-vectors/exec-remote-v1/README.md) documents
its encoding and examples.

See the [module guide](crates/motor-module/README.md) for placement and credential
setup, the [runner guide](crates/motor-runner/README.md) for transfer, admission,
retention, and isolation contracts, and the
[VM helper runbook](crates/motor-vm-helper/RUNBOOK.md) for the optional privilege
boundary and its separate acceptance tests.

## Standalone build and test

Dependencies come from crates.io and this repository, not an adjacent CortexKit
checkout. Ordinary checks and tests do not require a running SubC daemon or vault.
Install Rust, cargo-nextest, and Python 3, then run from the repository root. If
your shell injects Git configuration through `GIT_CONFIG_COUNT`, unset it first,
as some tests compare Git behavior exactly:

```sh
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
```

Build `ck-motor` on macOS and `motor-runner` on Linux:

```sh
cargo build --release -p motor-module --bin ck-motor --locked
cargo build --release -p motor-runner --locked
```

[AGENTS.md](AGENTS.md) lists formatting, vector and safety gates, platform
prerequisites, and operator tests that require a dedicated trusted host with
isolation accounts and privileged system services. Those host-isolation tests
and real macOS daemon integration are ignored in the ordinary suite; a passing
portable suite does not establish those boundaries.

## Vendored source

Motor's history was imported from prefrontal. The build-slot supervisor remains
vendored from prefrontal revision `d529e17eb`; its provenance and file digests are
in [VENDORED.md](crates/prefrontal-build-slot/VENDORED.md). The repository and its
vendored files are covered by the root [MIT license](LICENSE).
