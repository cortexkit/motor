# Architecture

Motor runs build and test commands for coding agents on remote machines,
against a snapshot of the caller's checkout placed at the same absolute path.
Linux jobs run in a tenant VM on a hosted server; Windows and macOS jobs each
run in a fresh virtual machine. Output streams back as it is produced, a job
survives dropped connections, and a command is never run twice: a job either
ran, provably did not start, or is reported as unknown.

Motor is the `exec-remote/v1` provider for the
[CortexKit](https://github.com/cortexkit) daemon. The daemon, its callers
(agent tooling such as AFT, agent-file-tools, and `ck motor run`) and the
broker that places Linux jobs, part of CortexKit's cloud service, are separate
projects. This document names the parts, traces a job through them, and lists
the rules that matter most when changing them. `STRUCTURE.md` is the directory
map and says where new code goes.

## Parts

| Part | Where | Runs on | Role |
| --- | --- | --- | --- |
| `ck-motor` | `crates/motor-module` | Caller's machine | CortexKit daemon module: admits callers, captures snapshots, asks the broker for a lease for each Linux job, reaches Windows and macOS runners over SSH, follows job streams. [docs/module.md](docs/module.md) |
| Protocol | `crates/motor-protocol` | Both ends | Framed client-runner protocol and codec; re-exports the caller types from `cortexkit-exec-remote-types`. [docs/wire-protocol.md](docs/wire-protocol.md) |
| Broker | Not in this repository | Cloud service | Issues signed leases, places each Linux job on a registered host (or an overflow sandbox), bills usage. Motor keeps a byte-pinned copy of its contract and vectors in `crates/motor-lease/contracts/cloud/`. |
| Lease library | `crates/motor-lease` | Used by `ck-motor`, `motor-host` and `motor-runner` | Lease tokens and signatures, without I/O; `motor-fake-broker` is a test broker. |
| `motor-host` | `crates/motor-host` | Hosted server, unprivileged | TLS admission for leases, relay to the tenant VM's runner, heartbeats, usage and metrics to the broker, drain, a SQLite store. [docs/hosted-host.md](docs/hosted-host.md) |
| `motor-vm-helper` | `crates/motor-vm-helper` | Hosted server, root | Owns the Firecracker tenant VMs: start, stop, suspend, restore, disks, CPU limits, network. Its per-job VM slot pool is not used by any runner backend. [docs/linux-microvm.md](docs/linux-microvm.md) |
| `motor-runner` | `crates/motor-runner` | Linux: a tenant VM or an overflow sandbox. Windows and macOS: the runner host | Admission, queueing, Git mirrors, workspaces, execution, retained output. One instance serves one platform. |
| Linux sandbox | `crates/motor-runner/src/isolation*`, `systemd.rs`, `network*.rs` | Wherever a Linux runner runs | Runs each Linux job as `motor-job` in a transient systemd service. [docs/job-isolation.md](docs/job-isolation.md) |
| Windows backend | `crates/motor-runner/src/windows/` | Windows runner host | One VM per job from a sealed baseline: QEMU on a Linux host, or Hyper-V on a Windows host. [docs/windows-jobs.md](docs/windows-jobs.md) |
| Windows guest agent | `crates/motor-winagent` | Inside the Windows VM | Receives the tree, runs the command as the guest's job account, reports its exit code. |
| macOS backend | `crates/motor-runner/src/macos/`, `crates/motor-macvm`, `crates/motor-macagent` | macOS runner host | One Virtualization.framework VM per job; `motor-macvm` drives the VM, `motor-macagent` runs the command inside it. [docs/macos-runner.md](docs/macos-runner.md) |
| Compile cache | `crates/motor-runner/src/compile_cache.rs`, `writer/` | Runner instance | Shared read-only sccache for Linux jobs, filled by trusted builds of the default branch. [docs/compile-cache.md](docs/compile-cache.md) |
| Scheduler | `crates/motor-runner/src/cpu.rs`, `engine.rs`, `disk.rs` | Runner instance | vCPU claims, CPU pinning, queue order, disk admission and cleanup. [docs/scheduling.md](docs/scheduling.md) |
| Build-slot supervisor | `crates/prefrontal-build-slot` | Inside Linux jobs | Compiler wrapper that limits concurrent compiler processes across the jobs on one machine. |
| Unused microVM parts | `crates/motor-vm-loader`, `crates/motor-vm-guest` | None | Snapshot-memory loader and one-command guest supervisor for per-job Firecracker VMs. No runner backend uses them. |

## How a job flows

1. **Request.** A caller sends `exec.run` to the CortexKit daemon, which
   verifies the caller and routes it to `ck-motor`. `ck-motor` checks the
   operation is allowed for that caller, the paths are a real checkout, and the
   platform and network request can be met. A caller without permission is
   refused at the operation; a run that fails the other checks ends as
   `refused_before_start` with `ran: none`, before any snapshot is uploaded.
2. **Snapshot.** `ck-motor` captures the checkout into a commit: `HEAD`, plus
   working-tree changes and untracked non-ignored files, under a cap on
   untracked content. The caller's checkout is not modified.
   [docs/snapshot-contents.md](docs/snapshot-contents.md)
3. **Connect.** For Linux, `ck-motor` asks the broker for a lease. The broker
   places the job on a registered host, preferring one where the checkout is
   already warm, and `ck-motor` connects over TLS to that host's `motor-host`,
   which relays the connection to the runner in the tenant VM. A lease refusal
   is a typed refusal; it never falls back to another path. If the broker
   places the job on an overflow sandbox instead, `ck-motor` reaches that
   sandbox's runner over the provider's SSH proxy. Windows and macOS jobs go
   to the configured runner over SSH. Both sides exchange `hello`.
4. **Admit.** `ck-motor` sends `have` to learn which commits the runner holds,
   then `run`. The runner checks platform, capacity, queue depth, drain state
   and disk space, and replies `accepted` or `refusal`.
5. **Transfer.** `ck-motor` streams Git bundles holding only the missing
   history. The runner verifies them into its per-repository mirror and pins
   the commits.
6. **Queue.** The job waits for its workspace and for capacity. Heartbeats
   report queue time; past its budget the client asks to withdraw it.
7. **Set up.** On Linux the runner checks the snapshot out at the caller's
   absolute path inside the job, verifies the tree hash and prepares
   dependencies as a trusted step; Windows and macOS stage the tree for the
   VM. On every platform the job sees tracked files read-only; files named in
   `return_paths` stay writable.
8. **Start.** The runner records the job as started on disk, sends `started`,
   then releases the command: a one-byte gate on the systemd service's stdin on
   Linux, a start frame to the guest agent in a VM. A crash before the record
   is written runs nothing.
9. **Run.** Output is persisted in pages and streamed as sequenced `output`
   records. Cancel or timeout kills the whole process tree.
10. **Finish.** The runner writes the `terminal` record, lists changed paths,
    returns files named in `return_paths`, releases the workspace, and keeps
    the job's history for 24 hours for `attach`. On a hosted server,
    `motor-host` reports the job's usage to the broker.

Between jobs, maintenance expires old history and idle workspaces, cleans up
disk below its free-space goal, retires Git mirrors unused for 30 days
(`crates/motor-runner/src/retirement.rs`), and runs compile-cache fills when
capacity is free.

## State

- **Runner.** An instance's own state lives under its state root: the job
  table, retained output, Git mirrors, the workspace catalog, sandbox runtime
  directories and the compile cache. Workspaces live at the callers' paths
  under the runner's workspace roots, on the same reflink-capable filesystem.
  In a tenant VM that filesystem is the VM's persistent job disk.
- **Host.** `motor-host` keeps leases, relay positions, billing records and
  revocation tombstones in a SQLite store; finished jobs are pruned after
  their usage is acknowledged. `motor-vm-helper` keeps each tenant's VM disk,
  image and records under its root-owned storage.
- **Caller.** `ck-motor` keeps an append-only journal of job records with
  periodic checkpoints. [docs/module.md](docs/module.md)

## Rules that bite

These hold across the codebase. Breaking one often still passes the portable
suite (`cargo nextest run --workspace`), because many of them are checked only
by the operator tests in the [server gate](docs/server-gate.md).

- **Never re-run a command that may have run.** Only `refused_before_start`
  proves no start. Lost acknowledgements, restarts, expired history and
  unknown values all mean "may have run".
  [docs/wire-protocol.md](docs/wire-protocol.md#5-refusals-and-the-no-start-proof)
- **Record before release.** A job's started state reaches disk before its
  command can start.
- **Additive protocol only.** New fields and capabilities are optional and
  omitted at their default, so the pinned older client in
  `scripts/compat-client-revision` keeps working. `scripts/mixed-version.py`
  checks both directions.
- **The broker contract is byte-pinned.** A change is made in the cloud
  service's own repository and published there first; Motor then re-pins its
  copy in `crates/motor-lease/contracts/cloud/`. Hosts accept a new token claim before
  the broker emits it.
- **Fail closed on isolation.** A missing account, ACL tool, `sudo` rule,
  firewall table or Windows layout check refuses startup or the job; nothing
  falls back to running as the runner's account or with weaker isolation.
- **Guest reports are untrusted.** What a tenant VM reports to `motor-host`
  can only reduce what the host offers; it never grants capacity or waives
  billing.
- **Trusted steps never run caller code.** Dependency fetches, JavaScript
  installs, compile-cache fills and Windows preparation use locked inputs,
  offline where possible, and never run package lifecycle scripts as the
  runner. Jobs only read the shared compile cache.
- **One job per workspace at a time.** Jobs for the same checkout queue behind
  each other; parallel work needs separate checkouts (worktrees).
- **Same filesystem for reflinks.** Workspaces are made from mirrors with
  copy-on-write file clones (reflinks, `FICLONE`), so mirrors, workspaces and
  build folders must share one filesystem that supports them, such as XFS;
  there is no copy fallback.
- **Absent is not zero.** Missing accounting, disk telemetry or health data is
  omitted, never reported as zero.
- **Operator lanes are not CI.** Isolation, network, Windows, macOS and
  microVM behaviour is proven only by the operator tests listed in
  [CONTRIBUTING.md](CONTRIBUTING.md#tests-that-need-an-operator-host).

## Documents

Architecture:

- [docs/module.md](docs/module.md): the `ck-motor` module: placement, callers,
  credentials, storage, direct runs.
- [docs/scheduling.md](docs/scheduling.md): vCPU claims, CPU allocation, queue
  order, queue wait, disk admission, retention.
- [docs/compile-cache.md](docs/compile-cache.md): the shared compile cache and
  its fills.
- [docs/linux-microvm.md](docs/linux-microvm.md): `motor-vm-helper`, the loader
  and the guest.

Contracts:

- [docs/wire-protocol.md](docs/wire-protocol.md): framing, frames, outcomes,
  refusals, sequence and reattach.
- [docs/job-isolation.md](docs/job-isolation.md): what a Linux job sees and can
  reach.
- [docs/snapshot-contents.md](docs/snapshot-contents.md): what a snapshot
  contains.
- [docs/windows-jobs.md](docs/windows-jobs.md): platform selection, paths, exit
  status, and the VM a Windows job runs in.
- `crates/motor-lease/contracts/cloud/runners-contract.md`: the broker
  contract.

Operator guides:

- [docs/hosted-host.md](docs/hosted-host.md): provisioning a hosted server, its
  tenant VM, draining and rollback.
- [docs/hosted-cutover.md](docs/hosted-cutover.md): moving Linux jobs to
  hosted servers.
- [docs/runner-install.md](docs/runner-install.md): installing a runner.
- [docs/drain-and-rollback.md](docs/drain-and-rollback.md): drain, deploy and
  roll back a runner.
- [docs/server-gate.md](docs/server-gate.md): the full pre-push gate.
- [docs/windows-image.md](docs/windows-image.md): building and qualifying the
  Windows baseline.
- [docs/macos-runner.md](docs/macos-runner.md): the macOS runner and its image.

Elsewhere: [STRUCTURE.md](STRUCTURE.md), [CONTRIBUTING.md](CONTRIBUTING.md),
[SECURITY.md](SECURITY.md), [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md),
and the [protocol corpus](test-vectors/exec-remote-v1/README.md).

## Documentation conventions

- One topic per file in `docs/`, named for its subject.
- Describe the code as it is. History, incidents and reviews stay out.
- Contracts number their rules, name the test that proves each rule, and say
  "not covered by a test" when none does.
- Use `code` formatting for identifiers, paths and commands.
- Absent and unknown are their own states; never document a missing value as
  zero.
- No production numbers, account names, host names, addresses or private
  paths. Defaults the code documents are fine.
