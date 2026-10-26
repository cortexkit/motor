# Architecture

Motor runs build and test commands for coding agents on a remote Linux build
server, or in a fresh Windows virtual machine per job, against a snapshot of
the caller's checkout placed at the same path. This document names the parts,
traces a job through them, and lists the rules that matter most when changing
them. Each part's details live in the linked documents.

## Parts

| Part | Where | Runs on | Role |
| --- | --- | --- | --- |
| `ck-motor` | `crates/motor-module` | Caller's machine | CortexKit daemon module: admits callers, captures snapshots, holds the SSH connection, follows job streams. [module.md](module.md) |
| Protocol | `crates/motor-protocol` | Both | Framed client-runner protocol and codec; re-exports the caller types. [wire-protocol.md](wire-protocol.md) |
| `motor-runner` | `crates/motor-runner` | Build server | Admission, queueing, Git mirrors, workspaces, execution, retained output. |
| Linux sandbox | `crates/motor-runner/src/isolation*`, `systemd.rs`, `network*.rs` | Build server | Runs each Linux job as `motor-job` in a transient systemd service. [job-isolation.md](job-isolation.md) |
| Windows backend | `crates/motor-runner/src/windows/` | Build server | Boots one QEMU VM per job from a sealed baseline. [windows-jobs.md](windows-jobs.md) |
| Windows guest agent | `crates/motor-winagent` | Inside the Windows VM | Receives the tree, runs the command as the guest's `motor-job`, reports its exit code. |
| Compile cache | `crates/motor-runner/src/compile_cache.rs`, `writer/` | Build server | Shared read-only sccache for jobs, filled by `motor-cache` from the default branch. [compile-cache.md](compile-cache.md) |
| vCPU allocator and scheduler | `crates/motor-runner/src/cpu.rs`, `engine.rs` | Build server | Exact vCPU claims, CPU pinning, queue order, disk admission. [scheduling.md](scheduling.md) |
| Caller-root mapping | `crates/motor-runner/src/caller_path.rs`, `caller_path/` | Build server | Stores a `/home` caller's checkout in a runner-owned directory and presents it at the caller's path. [job-isolation.md](job-isolation.md#38-caller-root-mapping) |
| Build-slot supervisor | `crates/prefrontal-build-slot` | Inside Linux jobs | Limits concurrent compiler processes across all jobs on the host. |
| Linux microVM components | `crates/motor-vm-helper`, `motor-vm-guest`, `motor-vm-loader` | Build server | Firecracker slot pool, UFFD loader and guest supervisor. Not used by any runner backend in this build. [linux-microvm.md](linux-microvm.md) |

## How a job flows

1. **Request.** A caller (AFT for agents, or `ck motor run` for scripts) sends
   `exec.run` to the CortexKit daemon, which verifies the caller and routes it to
   `ck-motor`. `ck-motor` checks the operation is allowed for that caller, the
   paths are a real checkout, and the platform and network request can be met.
   A caller without permission is refused at the operation; a run that fails
   the other checks ends as `refused_before_start` with `ran: none`, before any
   snapshot is uploaded.
2. **Snapshot.** `ck-motor` captures the checkout into a commit: `HEAD`, plus
   working-tree changes and untracked non-ignored files, under a 2 GiB cap on
   untracked content. The caller's checkout is not modified.
   [snapshot-contents.md](snapshot-contents.md)
3. **Connect.** `ck-motor` opens (or reuses) one SSH connection, authenticated
   by a vault-held key, and runs `motor-runner connect`, which bridges to the
   runner's Unix socket. Both sides exchange `hello`.
4. **Admit.** `ck-motor` sends `have` to learn which commits the runner holds,
   then `run`. The runner checks platform, capacity, queue depth, drain state and
   disk space, and replies `accepted` or `refusal`.
5. **Transfer.** `ck-motor` streams Git bundles holding only the missing
   history. The runner verifies them into its per-repository mirror and pins the
   commits.
6. **Queue.** The job waits for its workspace and for capacity. Heartbeats
   report queue time; past its budget the client asks to withdraw it.
   [scheduling.md](scheduling.md)
7. **Set up.** The runner checks the snapshot out at the caller's absolute path
   (Linux) or stages it as a tar for the VM (Windows), verifies the tree hash,
   and prepares dependencies as a trusted step. "The caller's absolute path"
   means `/Users/...` on hosts that store callers' checkouts there, and
   `/home/...` on a host configured with a caller root, which stores the
   checkout elsewhere and binds it back at that path.
8. **Start.** The runner records the job as started on disk, sends `started`,
   then releases the command: a one-byte gate on the systemd service's stdin on
   Linux, a `Start` frame to the guest agent on Windows. A crash before the
   record is written runs nothing.
9. **Run.** Output is persisted in pages and streamed as sequenced `output`
   records. Cancel or timeout kills the whole process tree.
10. **Finish.** The runner writes the `terminal` record, lists changed paths
    (not copied back), releases the workspace, and keeps the history for 24
    hours for `attach`.

Between jobs, maintenance expires old history and idle workspaces, cleans up
under disk pressure, and runs compile-cache fills when no job is waiting.

A repository's Git mirror that no job has used for 30 days is retired
(`MIRROR_UNUSED_MS` in `crates/motor-runner/src/retirement.rs`), on the build
server, in tenant VMs and in Boat sandboxes alike. Use is read from the
records that depend on the mirror (jobs, including use as a declared sibling;
workspaces; transfers; warm-build fills; have-reply pins) and kept durably in
the catalog (`mirror_used_ms`), so a restart neither resets the clock nor
counts as use. A retained job, an in-flight setup, transfer or prepare, a
running fill, a non-Head workspace, held refs or a running compaction keep the
mirror whatever its age. Retirement discards the repository's Head
workspaces, sibling checkout, warm-build record and fill checkout, and its
JavaScript install and manager caches, then the mirror, each by rename into
`trash` and a background purge, under the ref and state locks, and logs one
`retired unused repository` line listing what went. Shared caches (Cargo
registry, compile cache) and `git-deps` dependency mirrors are not per
repository and are left to disk-pressure eviction.

## State on the runner

An instance's own state lives under `<state-root>/<instance>/`: the job table
(one record per job), retained output, Git mirrors, the workspace catalog,
sandbox runtime directories, and the compile cache (`sccache/fleet`).
Workspaces themselves live at the callers' paths under the owned workspace
roots (default `/Users`), on the same reflink-capable filesystem. A caller root
is the exception: a `/home` caller's checkout is stored under the instance's own
host directory (for example `/srv/build/callers/main/home`) and only presented
at the `/home` path inside the job, while the catalogs keep caller spelling. The
catalogued records that name a mapped path live in `<state-dir>/caller-catalog.json`;
`catalog.json` keeps identity records and its bytes are unchanged for them.
Instances on one host are independent apart from machine-wide resources: CPUs,
the build-slot limit, and the Windows VM slot. Each caller root belongs to one
instance.

## Rules that bite

These hold across the codebase. Breaking one often still passes the portable
suite (the ordinary `cargo nextest run --workspace`), because many of them are
checked only by the operator tests in the [server gate](server-gate.md).

- **Never re-run a command that may have run.** Only `refused_before_start`
  proves no start. Lost acknowledgements, restarts, expired history and unknown
  values all mean "may have run". [wire-protocol.md](wire-protocol.md#5-refusals-and-the-no-start-proof)
- **Record before release.** A job's started state reaches disk before its
  command can start, so a crash at any point either runs nothing or leaves a
  record that says it may have run.
- **Additive protocol only.** New fields and capabilities are optional and
  omitted at their default, so the pinned older client in
  `scripts/compat-client-revision` keeps working. `scripts/mixed-version.py`
  checks both directions.
- **Fail closed on isolation.** A missing account, ACL tool, `sudo` rule,
  firewall table or Windows layout check refuses startup or the job; nothing
  falls back to running as the runner's account or with weaker isolation.
- **A caller root is fixed once it has records.** A start whose catalog or job
  records name a caller prefix the runner is not configured with refuses to
  start rather than guess; retiring a mapping is the explicit
  `forget-caller-root` command.
- **Trusted steps never run caller code.** Dependency fetches, JavaScript
  installs, compile-cache fills and Windows preparation use locked inputs from
  the default branch or the lockfile, offline where possible, and never run
  package lifecycle scripts as the runner.
- **One job per workspace at a time.** Jobs for the same checkout queue behind
  each other; parallel work needs separate checkouts (worktrees).
- **Same filesystem for reflinks.** Mirrors, workspaces and targets must share
  one reflink-capable filesystem; a copy fallback is not provided.
- **Absent is not zero.** Missing accounting, disk telemetry or health data is
  omitted, never reported as zero.
- **Operator lanes are not CI.** Isolation, network, Windows and microVM
  behavior is proven only by the operator tests listed in
  [CONTRIBUTING.md](../CONTRIBUTING.md#tests-that-need-an-operator-host).

## Documents

Architecture:

- [module.md](module.md): the `ck-motor` module: placement, callers,
  credentials, storage, direct runs.
- [scheduling.md](scheduling.md): vCPU claims, CPU allocation, queue order,
  queue wait, disk admission, retention.
- [compile-cache.md](compile-cache.md): the shared compile cache and its fills.
- [linux-microvm.md](linux-microvm.md): the Firecracker helper, loader and
  guest.

Contracts:

- [wire-protocol.md](wire-protocol.md): framing, frames, outcomes, refusals,
  sequence and reattach.
- [job-isolation.md](job-isolation.md): what a Linux job sees and can reach.
- [snapshot-contents.md](snapshot-contents.md): what a snapshot contains.
- [windows-jobs.md](windows-jobs.md): platform selection, paths, exit status,
  and the VM a Windows job runs in.

Operator guides:

- [runner-install.md](runner-install.md): installing the runner on a build
  server.
- [drain-and-rollback.md](drain-and-rollback.md): drain, deploy and roll back.
- [server-gate.md](server-gate.md): the full pre-push gate.
- [windows-image.md](windows-image.md): building and qualifying the Windows
  baseline.

Elsewhere: [CONTRIBUTING.md](../CONTRIBUTING.md), [SECURITY.md](../SECURITY.md),
[THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md), and the
[protocol corpus](../test-vectors/exec-remote-v1/README.md).

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
