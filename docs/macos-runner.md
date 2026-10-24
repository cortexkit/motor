# macOS runner

The macOS host-preparation API stages snapshots and resolves dependencies before
a job can be admitted to a VM. A preparation refusal is a refusal before start:
the job command has not run.

## Host preparation

### Bun lockfiles

Host preparation supports the text `bun.lock` format. Binary `bun.lockb` files
are refused before dependency fetching or publication because host preflight
does not decode the binary format. The refusal names the `bun.lockb` path.

In the caller checkout, convert the lockfile with:

```sh
bun install --save-text-lockfile
```

Commit `bun.lock` and remove the superseded `bun.lockb` from the submitted
snapshot before retrying. The runner does not convert lockfiles or execute
caller lifecycle scripts on the host; dependency installation uses
`bun install --frozen-lockfile --ignore-scripts`.

## Guest boot contract

`motor-macagent` runs as root in a single-job macOS VM. Its executable is built
for `aarch64-apple-darwin`. It uses `motor-winagent`'s shared framing and macOS
profile, version 1. The Windows profile remains version 4.

### Nonce disk and connection

The host attaches exactly two disks: the boot disk and a read-only raw nonce
disk. At byte zero the nonce disk contains this header, nonce and bounded map:

```text
MOTOR-NONCE-V2\n
<32 lowercase ASCII hexadecimal characters>\n
<4-byte big-endian length><JSON mount map>
```

Here `\n` means one newline byte, not two literal characters. Remaining disk
bytes are zero padding, to a fixed 69,632-byte disk. The JSON array has at most
64 entries and is at most 64 KiB: `[{"tag":"workspace","destination":"/Users/motor/repo","read_only":false}, ...]`.
The nonce is generated per job. There is no v1 reader or compatibility mode.
The guest reads the entire image into a 4096-byte-aligned buffer with one
sector-aligned read before parsing it; neither discovery nor the explicit-device
path probes a short header on a raw device. Every padding byte is checked.
The guest probes whole raw
disk nodes named `/dev/rdiskN` (decimal N, no partition suffix) and requires
exactly one matching magic header; it does not assume a disk number. A matching
header with invalid nonce bytes is an error. The raw and corresponding block
devices must not be readable by `motorjob`; the image must give that account an
unprivileged primary group, with no disk-access groups.

The root agent connects to `VMADDR_CID_HOST` (2), vsock port **5000**, and sends
`Hello { version: 1, nonce }`. The host must already be listening. It accepts
one session, authenticates the nonce and refuses further connections. The wire
sequence is `Hello` → host `TreeEnd { command }` → agent `Started` → zero or more
`Output` → `Exit { code }` or `Signal { signal }`. There is no `TreeReceived` or
host `Start`. The host may send `Cancel` after `TreeEnd` until terminal status.

### Shares and command execution

The workspace is mounted read-write at its caller's absolute path. Each sibling
is mounted read-only at its own caller path, preserving their relative layout.
The per-job cargo-home share is mounted read-write at
**`/opt/motor/job/cargo-home`**. No host state, prep tree or persistent cache is
shared. The VM helper writes the exact tag/destination/access map retained by
its job configuration to the nonce disk. Before connecting or sending Hello,
the root agent validates the complete map and creates missing parent directories
as root, refusing symlinks in every existing component. Workspace and sibling
destinations are normalized paths beneath `/Users/`; only the pinned cargo-home
destination is permitted outside that root. Destinations and tags must be unique,
destinations must not overlap, the workspace and cargo-home must be read-write,
and every sibling must be read-only. The nonce disk remains an unmounted raw,
read-only device, never a guest share.
The runner emits tags `workspace`, `cargo-home` and `sibling-N` (decimal N),
with at most 62 siblings so the complete map stays within its 64-entry bound.

The agent runs `/sbin/mount_virtiofs` with `nosuid,nodev` for every entry and `ro`
for siblings, then verifies the filesystem type, exact tag, destination and
read-only, nosuid and nodev flags
against the kernel's mount table. Unexpected virtiofs mounts also fail bootstrap.
Entry-specific failures identify the tag, destination and reason; map-wide
inventory failures report a general reason. Either exits without connecting, so
the host refuses the job on its pre-start Hello timeout. The agent never materializes
source trees. `mounts` portable tests inject the mount system; actual mounts and
kernel tag spelling still require VM qualification.

The agent runs `/opt/motor/bin/bash -c <command>` as `motorjob`, with caller cwd.
Before accepting a command, the root daemon clears and verifies its own
supplementary groups, treating failure as fatal. The child uses Command's gid/uid
settings before chdir, not a late pre_exec credential drop; no privileged
supplementary group can survive std's tolerated EPERM on group clearing.
It clears the inherited daemon environment, applies the Linux caller-env
allow-list, then pins `HOME=/Users/motorjob`, a fresh writable job `TMPDIR`,
`RUSTUP_HOME=/opt/motor/rustup`, `CARGO_HOME=/opt/motor/job/cargo-home`, and
`PATH=/opt/motor/cargo/bin:/opt/motor/bun/bin:/opt/motor/bin:/usr/bin:/bin:/usr/sbin:/sbin`.
`CARGO_TARGET_DIR` is unset.

After bash exits, its real exit or signal status is saved, and every process of
the job uid is SIGKILLed. Both output pipes must reach EOF within two seconds of
that kill. Root-owned disk spools separate pipe draining from host flow control:
forwarding every byte before terminal status has no deadline. A pipe that stays
open closes the session without a terminal frame (`output_incomplete`). Cancel
also kills the entire job uid, even while output forwarding is blocked.

### Agent launch options and portable checks

The root launch daemon normally needs no arguments. Trusted image configuration
may override:

- `--nonce-device /dev/rdiskN`: read this raw device instead of discovery;
- `--port 5000`: set the host vsock port;

These are not caller-controlled job options. The shell, job account and pinned
environment have no CLI override. The nonce reader/discovery and environment
tests, plus the Unix execution tests, run with `cargo test -p motor-macagent`.
Execution tests substitute a private process group for the whole guest uid, so
they cannot kill unrelated host processes. Real uid isolation, raw-device
permissions and vsock authentication still require VM qualification.

## Runner configuration and operator lane

Run `motor-runner serve --backend macos-vm --vm-config <file>`. On macOS every
other backend requires `--development-allow-host-backend`; production launchd
configuration must never use that override. The TOML file names absolute `image`
and `helper` paths, the sealed image's `toolchain_version`, and `[tools.cargo]`,
`[tools.rustc]`, `[tools.bun]`, `[tools.git]` tables, each with `path` and `sha256`.
Runner resource trees are beneath its state root, separate from its per-instance
durable output log. Records in `macvm-records` are fsynced before staging;
two `macvm-locks/slot-N.lock` files and one admission lock enforce capacity.
Controller descriptors (state-root, admission, workspace, canonical and prep)
are close-on-exec and use explicit unlock-on-drop ownership. CLOEXEC alone is
not a fork boundary: a child parked before exec retains the same flock open-file
description. A controller unlock releases every such inherited reference; the
guard only does that in its creating process. Slot fd 4 intentionally survives
the helper exec and is not an unconditional unlock-on-drop controller guard.
The runner explicitly unlocks its slot only after the durable stop/disk proof,
and the helper marks fd 4 close-on-exec once adopted so it cannot leak through a
later helper-side exec. The no-exec regression deliberately keeps a forked child
parked while releasing ownership, rather than relying on that child to exec.
An unproven stop retains both the record and VM slot. Once disk release is proven,
the runner fsyncs `slot: null` before releasing that slot. Filesystem cleanup is
separate: it restores owner rwx on owned directories through no-follow directory
capabilities, streams an iterative deletion walk, and caps depth at 64. Input
projection, copy-back and sync also use explicit heap stacks with that depth
limit; the input-copy buffer is one heap allocation, never a recursive frame.
Unremovable/deep trees are moved under
the job's recorded `quarantine/<job_id>` path (or remain at the recorded original
path if relocation fails). Their inventory remains for retry/operator attention,
but they hold neither a VM slot nor the workspace lock. Quarantine remains visible
as degraded status and a warning; direct runner admission can still use free slots.
Recovery ignores unrelated/temp record-directory entries, while valid ownership
records with invalid resource paths still fail closed.

The five job-level ignored `macos_qualification_*` integration tests run on the VM host
against the **installed** launchd daemon through `ck motor run --platform macos`,
not a test-owned runner or an SSH command shell. Set
`MOTOR_MACOS_QUALIFICATION_WORKSPACE`, `MOTOR_MACOS_QUALIFICATION_STATE_ROOT`, and
`MOTOR_MACOS_QUALIFICATION_CRATE` to the disposable qualification fixture and
named Motor crate. `MOTOR_MACOS_QUALIFICATION_CK` optionally names the installed
client. The isolation lane requires one sibling fixture named by
`MOTOR_MACOS_QUALIFICATION_SIBLING`; that path is passed to every job as an explicit
`--sibling` snapshot. The isolation lane checks the exact three virtiofs mounts,
fails a sibling write and raw-disk reads as motorjob, and compiles a guest-only
vsock connection probe using the sealed image's Command Line Tools. The env lane
checks that tool version against `script/macos-runner/toolchain.json`. The isolation
lane also observes a guest-written file in the live host job tree and checks its
motor ownership.
Run host qualification as `motor`, with `MOTOR_MACOS_QUALIFICATION_EVIDENCE`
naming a persistent writable evidence directory outside build/cache trees. The
installed client must include `--record-file`: every job retains credential-free
Accepted/Started/Output/Terminal JSONL, CLI streams and exit status. Concurrent
builds also retain independent guest streams/status before job-tree cleanup;
`script/macos-runner/qualification.py` compares bytes, sequence order and status.
Set `MOTOR_MACOS_QUALIFICATION_WORKSPACE_SECOND` to a second canonical Git root
for concurrency. The lane rejects two cwds in one checkout and counts only the
two Accepted job IDs' live recorded services, never ambient VMs.
Keep the provider's existing SYNAPSE workload (its `synapse-work` checkout) active
for the concurrent lane, and retain the provider operator's confirmation that it
ran throughout the measurement. This deliberately measures memory and swap while
the shared Mac also serves its other workload. Start with cold target/prep caches
and retain the concurrent test's timing,
minimum free memory, swap observations and service owners. The nonce permission
probe uses an aligned 4096-byte read and accepts only EACCES or EPERM, never an
alignment failure such as EINVAL. The Bun fixture must have
locked local sibling dependencies and a darwin-arm64-only package.
`script/macos-runner/fixtures.py <new-directory-under-/Users>` supplies committed
Bun/Cargo sibling fixtures and a pinned Darwin-arm64 esbuild package. The Bun
lane executes that package and both sibling dependencies, and compares lock
hashes. The generator also supplies positive/missing-commit private dependency
fixtures and an isolated client Cargo cache. Run
`macos_qualification_private_bundle` with `_PRIVATE_GOOD`, `_PRIVATE_MISSING` and
`_PRIVATE_URL` variables prefixed by `MOTOR_MACOS_QUALIFICATION`; configure the
reported Cargo cache in the installed ck-motor service, not just the test shell.
The positive job builds offline; the negative must refuse before Started and
name the source. A host packet capture with a known control connection must
separately prove prep did not contact the unreachable source. Fixture setup,
cache requirements and evidence commands are in
[`script/macos-runner/README.md`](../script/macos-runner/README.md#operator-evidence-tools).

## Host helper and sealed-image layout

`motor-macvm` is the VM-creating binary; the installer must sign it with the
virtualization entitlement. The library's `JobVmConfig` cannot request networking
or graphics. `job_configuration_has_exact_devices_and_share_access` checks the
portable plan, and `framework_job_devices_match_portable_plan_without_booting_vm`
checks its actual framework device arrays on macOS without starting a VM.
Workspace and cargo-home shares are read-write; siblings are read-only. Tags and
guest destinations are retained in the plan and written to the boot metadata
disk. The root guest agent mounts that mapping before its Hello.

A sealed directory contains four regular read-only files: `disk.img`,
`auxiliary-storage`, `hardware-model` (the framework's data representation), and
`machine-identifier` (its data representation). The directory itself is
read-only. `clone_image` makes an APFS `clonefile` copy with no ordinary-copy
fallback and makes only the clone's boot disk and auxiliary storage writable.
The runner must record the clone and nonce paths durably **before** creating them;
creation failures intentionally leave any partial resource for recorded cleanup.
`create_nonce_disk` exclusively creates a 69,632-byte, owner-readable, read-only
raw image with the nonce format above, zero padding, and fsyncs it and its parent.

`spawn_job_helper(executable, plan, slot_file)` starts one helper per VM, blocked
on its stdin gate. The caller retains the slot lock; an inherited descriptor
keeps the same lock held in the helper. The runner records the helper PID and
kernel start timestamp and fsyncs its gate-opening intent before `open_gate()`.
EOF before `G` never starts a VM. After `G`, closing/dropping the gate force-stops
the VM without guest cooperation; the helper waits for a framework stop
acknowledgement before exiting. `gate_eof_before_open_never_starts` and
`gate_eof_after_open_waits_for_vm_stopped` pin these transitions.

Helper stdout carries newline-delimited `HelperReport` JSON. `Started` carries
both helper and service `ProcessIdentity` values (PID, kernel start seconds and
microseconds). The runner must fsync the service identity before declaring the
job Started. The returned Unix stream carries the guest's shared framing, with
its authenticated Hello forwarded first. The first connection consumes the
listener even if Hello is bad, missing, or the wrong version; later connections
are refused. A bad Hello fails/stops the job, not just the connection. The tests
`single_session_accepts_only_correct_first_hello` and
`bad_hello_consumes_session_and_cannot_retry` cover that policy. The helper CLI's
`job <request-json>` entry requires the inherited session descriptor 3 and slot
descriptor 4; callers should use `spawn_job_helper`, not assemble those manually.

### Service identity and stop proof

A helper owns exactly one `VZVirtualMachine`. The service identity is established
using same-effective-UID process enumeration, the executable in
`Virtualization.framework` named `com.apple.Virtualization.VirtualMachine`, and a
match of the clone boot disk's **device and inode** against its open vnode
references. The scan checks both descriptors (`PROC_PIDLISTFDS` /
`PROC_PIDFDVNODEPATHINFO`) and fileports. Exactly one inspectable service must
match. If the private `responsibility_get_pid_responsible_for_pid` symbol is
available through `dlsym`, its answer must also name this helper; a restricted or
failed lookup is an error, not permission to guess. With no symbol, the public
vnode match remains mandatory. `service_identity_requires_exactly_one_inspectable_disk_owner`
and `incomplete_and_denied_libproc_lists_fail_closed` check the fail-closed rules.

Cleanup closes the gate and allows the helper 600 ms to stop gracefully before
SIGKILL escalation; the overall stop/proof polling budget is 1.4 s. It then calls
`process::release_proven` until the stop
bound expires. It requires both that the recorded service PID/start identity is
no longer alive and that no inspectable same-UID process holds the clone boot
disk, auxiliary storage or nonce disk by descriptor or fileport. Query errors,
partial scans and missing disk paths are **not** proof. Do not delete the paths
or reuse the slot if proof fails; retain the record and report degraded for
operator attention. With no reported service, the same descriptor/fileport scan
is still mandatory. After helper loss, a same-UID framework service that holds
this job's exact boot-disk vnode can be stopped without guessing from its name or
PID alone. Unrelated services are never killed. Unrecorded motor services and
unknown disk holders keep the stop unproven until they disappear. Persisted proof
is versioned; legacy proofs are revalidated before deletion or slot reuse.

Direct unreaped helpers are terminated through their retained child handles.
Darwin has no pidfd-style public signal operation in use here for services or
restart recovery. PID/start observations are checked twice, including immediately
before numeric-PID SIGKILL, but they are not an identity-atomic signal: exit and
PID reuse between the last observation and kill remains a residual race. The
timestamp check must not be described as proof against that interval.

A global `proc_listpidspath` scan is deliberately not used: it silently skips
processes it cannot inspect. Exclusive file locks are advisory and cannot prove
release either. The narrower same-UID proof relies on a qualification assumption:
the framework service and every process receiving its disk references run as
`motor`. No other UID may receive those references, and no process may reopen
these recorded paths while cleanup checks them. A live process that cannot be
inspected prevents cleanup rather than becoming a false negative.

### Image-build subcommand and operator qualification

`motor-macvm image-build <image-directory> [<restore.ipsw>]` is separate from the
job API. It alone adds a display, keyboard, pointer and NAT adapter, and presents
the guest in a window for Setup Assistant. The image recipe supplies a writable
bundle with a sized boot disk and the restore image's compatible hardware model,
machine identifier and auxiliary storage. With an IPSW argument the helper uses
`VZMacOSInstaller`; otherwise it boots that bundle. Closing the window force-stops
the VM. The recipe seals the bundle after provisioning; job helpers never accept
an image-build option.

The following checks require the installed daemon and a dedicated operator host;
they are not portable tests and are not run by ordinary crate tests:

- `macos_qualification_helper_identity`: start one job through the installed
  daemon and prove its helper reports its own same-`motor`-UID service holding
  its clone disk. This single-helper lane does not prove two-helper disambiguation.
- `macos_qualification_helper_sigkill`: SIGKILL a helper with a running VM; verify
  its service exits and releases boot, auxiliary and nonce vnode references
  within two seconds. Prove `release_proven` succeeds only after release. If this
  fails, retain resources/slots and choose an operator-approved fallback before
  rollout; never replace proof with a host-wide VM count or advisory lock.
- `macos_qualification_helper_gate`: drive the exact installed signed helper
  directly, closing gate stdin before and after opening it; no VM may start in
  the first case, and EOF must stop the second VM before cleanup escalation.
  This tests the gate primitive, not an actual runner SIGKILL.
- `macos_qualification_isolation`: deny guest raw-disk reads and guest vsock
  access, and check exact mounts, guest identity and host file ownership. There
  is no separate `macos_qualification_nonce_channel` integration test.

### Audit limits retained deliberately

There are no quotas on the read-write shares, boot-clone growth, quarantine or
root guest output spools. The 20/40 GiB admission/eviction floors are host-wide
pressure policy, not per-job byte limits; a hostile job can still consume state
volume space. Operators must monitor the volume and quarantined inventories.

The target store is untrusted guest output and is published after non-cancelled
failed jobs as well as successful ones, including signals or an incomplete agent
session once VM disk release is proven. This deliberately matches Linux, where a
workspace retains its target between jobs. The case-insensitive workspace key is
not a branch/principal integrity boundary: later jobs may consume earlier build
artifacts. The host only clones, swaps and removes that data; it never executes
or parses it. Neither cache integrity partitioning nor quotas are provided here.

### Additional rented-Mac/guest audit qualification

Run `macos_qualification_helper_identity` and `macos_qualification_helper_sigkill`
as the motor account against the installed daemon, with the existing workspace,
state-root and client variables plus `MOTOR_MACOS_QUALIFICATION_INSTANCE`. They
observe the daemon's real helper/service record and disk-vnode association; the
SIGKILL lane requires both processes gone within two seconds. Run
`macos_qualification_helper_gate` with `MOTOR_MACOS_QUALIFICATION_HELPER` and
`MOTOR_MACOS_QUALIFICATION_VM_CONFIG`. It first derives the configuration from a
live daemon helper's parent argv, checks the live executable matches that exact
helper path, rejects build-tree paths, and verifies its signature and virtualization
entitlement. The direct signed-helper primitive then tests both EOF-before-G and
EOF-after-Started, using the sealed image named by that daemon configuration.
The latter is the proof of the after-open gate promise: the helper must exit
successfully with `Stopped { reason: GateEof }`, and its VM/service/disk references
must be gone within two seconds of EOF, before any escalation exists. Cleanup
fallback runs only after the proof passed or failed and retains the inventory if
release cannot be established. It never turns a failed EOF proof into a pass.
All eight host qualification tests serialize their lanes, except the concurrent
lane's deliberate two-job pair. Real job qualification remains daemon-based;
this separately authorized primitive directly drives the installed helper so
that daemon cancellation escalation cannot mask an EOF failure. The native macvm
`disk_holder_blocks_release_without_reported_service` test checks the real
same-UID libproc scan with no reported service identity.

Inside a disposable guest, as root with `MOTOR_MACOS_GUEST_QUALIFICATION=1`, run
`macos_qualification_drop_precedes_cwd_resolution`. It verifies that the standard
Command uid/gid transition (which clears supplementary groups before chdir)
rejects a root-only cwd rather than resolving it while privileged. Both uid 0 and
primary gid 0 are refused for motorjob. These native paths must be compiled/linted
and exercised on the rented Mac/guest; Linux does not cross-lint Apple-only code.
