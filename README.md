# Motor

Motor runs build and test commands for coding agents on a remote Linux build
server, or in a fresh Windows virtual machine per job. A client module on the
caller's Mac captures a snapshot of the checkout, including uncommitted work,
sends only the Git history the server lacks, and the server checks it out at
the same absolute path and runs the command in an isolated sandbox. Output
streams back as it is produced, jobs survive dropped connections, and a command
is never run twice: a job either ran, provably did not start, or is reported as
unknown. Motor is the `exec-remote/v1` provider for the
[CortexKit](https://github.com/cortexkit) daemon; the daemon and its callers
are separate projects.

## Crates

| Crate | Role |
| --- | --- |
| [`motor-module`](crates/motor-module/README.md) | `ck-motor`, the client module: caller admission, snapshots, SSH transport, `ck motor run`. |
| `motor-protocol` | The framed client-runner protocol and codec; re-exports the caller types from `cortexkit-exec-remote-types`. |
| [`motor-runner`](crates/motor-runner/README.md) | The build-server daemon: admission, scheduling, workspaces, Linux sandbox, Windows VM backend, compile cache. |
| `motor-winagent` | The guest agent inside each Windows job VM. A standalone crate outside the workspace. |
| [`motor-vm-helper`](crates/motor-vm-helper/README.md) | Root service owning a pool of Firecracker VM slots. Not used by any runner backend in this build. |
| [`motor-vm-loader`](crates/motor-vm-loader/README.md) | Static `userfaultfd` snapshot-memory loader for those VMs. |
| [`motor-vm-guest`](crates/motor-vm-guest/README.md) | Supervisor that runs one command inside such a VM. |
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

Release builds: `ck-motor` on macOS, `motor-runner` on Linux.

```sh
cargo build --release -p motor-module --bin ck-motor --locked
cargo build --release -p motor-runner --locked
```

[CONTRIBUTING.md](CONTRIBUTING.md) lists the full CI gate, the server gate, and
the tests that need a privileged operator host. A passing portable suite does
not prove the isolation boundaries; those tests run only on a dedicated server.

## Documentation

Start with [docs/architecture.md](docs/architecture.md): the parts, how a job
flows through them, and links to every contract and operator guide. Security
reports go through [SECURITY.md](SECURITY.md).

## License

MIT; see [LICENSE](LICENSE). Adapted third-party material is listed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
