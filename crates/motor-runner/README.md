# motor-runner: Linux job isolation

`motor-runner` receives workspace snapshots from `ck-motor`, maintains Git
mirrors, queues jobs, and retains output for replay. It speaks motor's internal
client-runner protocol over SSH and a local Unix socket; it is not the
caller-facing `exec-remote/v1` API. See the [module guide](../motor-module/README.md)
for that integration and the [protocol corpus](../../test-vectors/exec-remote-v1/README.md)
for wire examples.

On Linux, `serve` enables job isolation. Missing isolation prerequisites refuse
startup or the affected job; they do not permit execution as the runner's account.
`Config::new` uses the portable in-process backend for tests. That backend is not
an isolation boundary.

## Installation prerequisites

Choose the runner service account, state root, workspace roots, capacity, and
storage budget for the installation; no host inventory is prescribed here.

- Use native reflink-capable storage for state, workspace copies, and target
  copies. Reflink source and destination must be on the same filesystem; XFS with
  reflink support is suitable. A cross-filesystem copy or an ext4 scratch directory
  is not a substitute for the reflink path.
- Provision the `motor-job` account expected by the job backend. It must be a
  distinct non-root, non-administrative, no-login account, not the runner account.
- If shared-cache fills are enabled, provision the separately validated
  `motor-cache` account. It must differ from root, the runner, and `motor-job`, and
  must also be non-administrative and no-login. Invalid cache-writer provisioning
  disables fills rather than making foreground jobs depend on that account.
- Provide systemd 259 or newer with private PID namespace support, Linux namespace
  and mount support, sudo delegation for the runner's service-management operations,
  and POSIX ACL tools. The job and cache accounts must not receive administrative
  authority. Missing job-account, ACL, privilege-delegation, namespace, or mount
  support fails closed.
- Install the required toolchains and command-line tools before running offline
  jobs. The [tool list](../../script/runner/tools.txt) describes tool installation
  inputs and optional integration prerequisites. SSH transport needs OpenSSH on
  the client and an SSH service on the runner host.

The optional VM helper is a separate privilege boundary. Installing it does not
select a VM execution backend; see its [runbook](../motor-vm-helper/RUNBOOK.md).

## Transfer progress bound

Bundle upload and mirror import have a 60-second no-progress bound, not a total
transfer-duration limit. A transfer that keeps making progress can run longer.
Workspace setup has neither this transfer bound nor a queue-wait timeout.

A stalled transfer ends with `transfer_stalled`, `refused_before_start`, and
`ran: none`. That before-start proof, rather than a disconnect or missing output,
allows a caller to choose a local fallback. The executor itself does not fall
back locally. `history_expired` and `outcome_unknown` are not proofs that a command
did not run and must not trigger resubmission.

## Stream response and queue-clock lifecycle

The live job stream carries output, terminal records, and heartbeats. Optional
`heartbeat.queue_wait_ms` is present only while the job waits for execution
permits. Its clock starts after bundles have been verified, fetched, and pinned,
not at acceptance. It is absent once the job owns capacity for setup or execution.
Clients clear their local queue deadline when that timing is absent or the stream
is disconnected; a plain heartbeat does not authorize withdrawal.

Queue-wait withdrawal is valid only in the permit queue. Upload, fetch, and setup
are not queue wait. `withdraw.reason: transfer_stalled` is a separate before-dispatch
withdrawal. A withdrawal answer, not a client-side timeout, establishes whether the
job was withdrawn or had already started.

Started records are negotiated through Hello's optional `started` capability.
Legacy peers retain contiguous output sequences and projected replay cursors.
Output eviction still exposes a truncation boundary. Attach resumes retained
records rather than submitting a second command.

## Completed transfer retention

Verified snapshot and integration-tip refs, completed prepare refs, and durable
job/replay records have 24-hour retention windows. Active transfers and recorded
workspaces additionally pin the commits they need. Ref retention is distinct from
workspace-directory lifetime; callers cannot assume that an expired transfer
record or an evicted workspace can supply old output or Git objects indefinitely.

Task workspaces may expire when idle. A workspace marked `workspace_kind: head`
survives idle expiry, but is still eligible for disk-pressure LRU eviction. Older
peers that omit the field retain task lifetime.

## Thin history negotiation

Clients negotiate available commits with `have`/`have_reply` and send the missing
Git history in bundles. Mirror objects needed by accepted snapshots and integration
tips remain pinned during their retention window. A shared tip must not be treated
as a guarantee that all history needed by another snapshot is available; transfers
must remain valid after prior transfer records and workspace refs expire.

## Admission and queue-wait diagnostics

### Disk admission

Disk admission measures bytes available to the daemon on the instance state
filesystem, not the apparent size of a workspace. `Config::free_space_floor_bytes`
or `serve --free-space-floor-bytes N` configures the hard floor. The default is
128 GiB (`137438953472` bytes); zero disables the guard. The soft cleanup goal is
twice the configured floor.

Falling below the soft goal triggers proactive cleanup, but does not by itself
refuse work. If cleanup leaves free space below the hard floor, admission refuses
before upload or command start with `runner_disk_full` and
`disk_full: {free_bytes, floor_bytes}`. Exactly at the floor still admits. Queued
work is checked again before setup.

Cleanup is limited to eligible catalogued workspace roots and instance-owned
staging. The default allowed source root is `/Users`; add roots with
`--workspace-root PATH`. Active or reserved workspaces, source trees, mirrors,
and writer paths are protected. Cleanup does not traverse symlinks or mounts,
and does not delete individual compiler-cache entries. Whole-cache eviction is
fenced against writer activity.

### Capacity and queueing

Running jobs and workspace setups both count against `max_weight` (default 28;
set `serve --max-weight N` for the installation). `weight_hint` is the job's exact
vCPU claim; without a hint it claims two. Compiler flags and environment concurrency
settings do not enlarge that claim. The isolated service enforces its allocated
CPU and memory limits.

Among workers, the oldest job in each workspace is its queue head. For the first 60 seconds,
smaller heads from other workspaces may backfill when a head cannot fit. After
that grace period, acceptance order takes priority: an aged runnable head waiting
for more permits stops backfill. This bounds overtaking, not job runtime.
Queue-wait diagnostics and withdrawal remain separate from execution timeout.

Direct-caller runs (`ck motor run`) receive a 30-minute queue-wait budget. With
negotiated `gate_jobs` support, gates precede workers in arrival order; agents
cannot select this class. A capacity-blocked gate immediately fences smaller
jobs, but workspace occupancy, deletion and sibling conflicts skip the gate so
unrelated workspaces can proceed. Running jobs are not stopped or assigned
different CPUs, and no capacity is permanently reserved. Older runners receive
no class mark.

Completed unit-backed jobs may persist `reservation_use` with CPU time, execution
wall time and `average_use_ppm` (1,000,000 means 100%). `runner.health` lists retained
jobs over ten minutes below 25% use, with workspace and vCPUs. CPU totals come
from native `systemd-run --wait` summaries at millisecond precision; unavailable
accounting is omitted and never delays Terminal.

## Boundary

The trusted runner uses its service account; foreground jobs run as `motor-job`
and dependency fills as `motor-cache`. Both use transient system services with
service UUIDs independent of caller job IDs.

A job service has an empty root, strict system protection, a private home, no new
privileges or capabilities, and private PID, network, and device namespaces. Its
workspace and private target are writable. Source snapshots, toolchains, required
system files, shared Cargo registry/Git inputs, and checkout Git metadata are
read-only. Host state, mirrors, other workspaces, and the operator's home are not
exposed wholesale. Job networking is loopback-only.

Trusted preparation fetches locked sources from daemon state, not from the
uploaded working directory. Builds and build scripts execute only after the
privilege drop. Jobs may modify their own files and disrupt their own cleanup;
they cannot acquire mounts or privileges. This is a filesystem and credential
boundary, not a microVM or an untrusted-kernel boundary.

## Shared compiler cache and trusted fills

The optional shared sccache tree lives at
`<state-root>/<instance>/sccache/fleet`. `--sccache-size-gib` sets its cap; the
default is 200 GiB. `motor-cache` owns it. Isolated job readers receive an
optional read-only bind and cannot populate it, including through build scripts
or proc macros. A missing cache does not prevent job startup. Each workspace
keeps its own target at `/run/motor/target`; target outputs are not shared between
workspaces. Readers need sccache 0.18.0 with the
[read-only lookup patch](../../script/runner/patches/sccache-0.18.0-read-only-hits.patch).

A fill uses the declared warm command or `--base-command` override against an
approved integration tip from the instance mirror, never a submitted branch
snapshot, environment, or sibling checkout. Trusted preparation fetches locked
sources with compiler wrappers unset. The writer then runs offline as
`motor-cache`; a checked-in lockfile change fails the fill.

A descriptor covers the toolchain, warm command, and relevant manifest, lock,
config, and toolchain files. Miss-triggered refills require the submitted
snapshot's descriptor to match the trusted tip's descriptor and occur at most
once per repository/tip/descriptor. Fills are serialized per instance and start
only when no foreground job is queued. The cache fence lasts through writer
shutdown and ACL repair. Writer scratch is discarded, not published as a shared
target.

## Drain and restart recovery

A service restart is not a completion barrier. Use the drain protocol to stop
admission while accepted work settles. The drain timeout defaults to 1800 seconds;
`serve --drain-timeout-s N` configures the signal-driven deadline, and SIGUSR1
initiates drain. `motor-runner drain --instance <instance> --drain-timeout-s N`
requests it over the control protocol.

During drain, new runs and prepares receive `runner_draining` before start.
Accepted uploads, queued jobs, in-flight setups, running jobs, and existing fills
continue; no new fills are created. The daemon exits when work is terminal or
the deadline expires. A deadline exit does not assert that jobs finished.
Concurrent requests share the earliest deadline: a later request may shorten,
never extend it, and the first request decides whether the daemon exits.
`--exit-after-drain false` holds the daemon instead;
`motor-runner resume --instance <instance>` reopens admission.

Startup examines the durable job table before accepting connections. Awaiting-
bundle or queued records with neither a launch identity nor a start timestamp become a
`lost_to_restart` before-start refusal with `ran: none` and the original job ID.
Jobs with a durable launch identity or start timestamp are never given a no-start
refusal that would authorize resubmission. Surviving system units are reattached
with their leases and cancellation/timeout behavior. Isolated jobs
persist output and exit status in private sandbox files for replay after restart.
A dead unit without a durable exit code remains `outcome_unknown`.

## Verification

Build and run the ordinary runner suite from the repository root:

```sh
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo build --release -p motor-runner --locked
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run -p motor-runner --locked
```

See [AGENTS.md](../../AGENTS.md) for the workspace gates and platform prerequisites.
Portable tests do not establish the Linux service isolation boundary. The ignored
`isolated_` tests require a trusted, dedicated Linux host with the accounts,
reflink storage, systemd, ACL, and privilege-delegation prerequisites above. They
are not hosted-CI jobs and must not run on a machine holding unrelated workloads.
The VM helper's root-only and real-VM acceptance lanes are separate from those
runner tests and from the portable suite.
