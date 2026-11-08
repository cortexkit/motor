# Motor

Motor runs build and test commands for coding agents on remote machines: Linux
jobs in a tenant VM on a hosted server, Windows and macOS jobs each in a fresh
virtual machine. A client module on the caller's own machine captures a
snapshot of the checkout, including uncommitted work, sends only the Git
history the runner lacks, and the runner checks it out at the same absolute
path and runs the command in an isolated sandbox. Output
streams back as it is produced, jobs survive dropped connections, and a command
is never run twice: a job either ran, provably did not start, or is reported as
unknown. Motor is the `exec-remote/v1` provider for the
[CortexKit](https://github.com/cortexkit) daemon; the daemon and its callers
are separate projects.

## Crates

| Crate | Role |
| --- | --- |
| [`motor-module`](crates/motor-module/README.md) | `ck-motor`, the client module: caller admission, snapshots, broker leases for Linux jobs, SSH to Windows and macOS runners, `ck motor run`, `attach` and `cancel`. |
| `motor-protocol` | The framed client-runner protocol and codec; re-exports the caller types from `cortexkit-exec-remote-types`. |
| `motor-lease` | Lease tokens and signatures, and the byte-pinned copy of the broker contract. |
| [`motor-runner`](crates/motor-runner/README.md) | The runner daemon: admission, scheduling, workspaces, Linux sandbox, Windows and macOS VM backends, compile cache. |
| [`motor-host`](crates/motor-host/README.md) | The hosted server agent: TLS admission for leases, relay to the tenant VM's runner, reports to the broker. |
| [`motor-vm-helper`](crates/motor-vm-helper/README.md) | Root service owning the Firecracker tenant VMs on a hosted server. |
| `motor-macvm`, `motor-macagent` | The macOS job VM driver and the guest agent inside each macOS job VM. |
| `motor-winagent` | The guest agent inside each Windows job VM. A standalone crate outside the workspace. |
| [`motor-vm-loader`](crates/motor-vm-loader/README.md), [`motor-vm-guest`](crates/motor-vm-guest/README.md) | Snapshot-memory loader and one-command guest supervisor for per-job Firecracker VMs. Not used by any runner backend. |
| `prefrontal-build-slot` | Compiler-slot supervisor that limits concurrent compiles across jobs; vendored, see [VENDORED.md](crates/prefrontal-build-slot/VENDORED.md). |

## Build and test

The portable suite needs Rust, [cargo-nextest](https://nexte.st) and Python 3;
some runner tests need a few more tools, listed below the commands. It needs
no running daemon, vault or build server. If your shell injects Git configuration
through `GIT_CONFIG_COUNT`, clear it first, because some tests compare Git
behavior exactly:

```sh
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
python3 crates/motor-protocol/tests/generate_vectors.py --check
```

On Linux, point `TMPDIR` at a reflink-capable filesystem such as XFS; runner
tests clone workspaces with reflinks. The runner's JavaScript tests also need
Node, npm, Bun and pnpm, and the SSH transport test needs `sshd`.

Release builds pass the commit in `MOTOR_BUILD_COMMIT`; the `ck-motor` release
binary is built with `scripts/build-ck-motor-card.sh`, which records that
provenance. [AGENTS.md](AGENTS.md) has the commands.

[CONTRIBUTING.md](CONTRIBUTING.md) lists the full CI gate, the server gate, and
the tests that need a privileged operator host. A passing portable suite does
not prove the isolation boundaries; those tests run only on a dedicated server.

## Documentation

Start with [ARCHITECTURE.md](ARCHITECTURE.md): the parts, how a job flows
through them, and links to every contract and operator guide.
[STRUCTURE.md](STRUCTURE.md) maps the directories and says where code goes. Security
reports go through [SECURITY.md](SECURITY.md).

## License

MIT; see [LICENSE](LICENSE). Adapted third-party material is listed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
