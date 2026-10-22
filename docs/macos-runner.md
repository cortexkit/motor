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
disk. At byte zero the nonce disk contains these bytes, with no length prefix:

```text
MOTOR-NONCE-V1\n
<32 lowercase ASCII hexadecimal characters>\n
```

Here `\n` means one newline byte, not two literal characters. Remaining disk
bytes are padding. The nonce is generated per job. The guest probes whole raw
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
shared. The host/image setup mounts these before sending `TreeEnd`; the agent
does not materialize or mount shares.

The agent runs `/opt/motor/bin/bash -c <command>` as `motorjob`, with caller cwd.
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
- `--cargo-home /absolute/guest/mount`: set the cargo-home mount point.

These are not caller-controlled job options. The shell, job account and pinned
environment have no CLI override. The nonce reader/discovery and environment
tests, plus the Unix execution tests, run with `cargo test -p motor-macagent`.
Execution tests substitute a private process group for the whole guest uid, so
they cannot kill unrelated host processes. Real uid isolation, raw-device
permissions and vsock authentication still require VM qualification.

## Host helper and sealed-image layout

`motor-macvm` is the VM-creating binary; the installer must sign it with the
virtualization entitlement. The library's `JobVmConfig` cannot request networking
or graphics. `job_configuration_has_exact_devices_and_share_access` checks the
portable plan, and `framework_job_devices_match_portable_plan_without_booting_vm`
checks its actual framework device arrays on macOS without starting a VM.
Workspace and cargo-home shares are read-write; siblings are read-only. Tags and
guest destinations are retained in the plan. The image's root mount setup must
consume that mapping before the host sends `TreeEnd`.

A sealed directory contains four regular read-only files: `disk.img`,
`auxiliary-storage`, `hardware-model` (the framework's data representation), and
`machine-identifier` (its data representation). The directory itself is
read-only. `clone_image` makes an APFS `clonefile` copy with no ordinary-copy
fallback and makes only the clone's boot disk and auxiliary storage writable.
The runner must record the clone and nonce paths durably **before** creating them;
creation failures intentionally leave any partial resource for recorded cleanup.
`create_nonce_disk` exclusively creates a 4096-byte, owner-readable, read-only
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

Cleanup must close the gate, wait for the helper to exit (SIGKILL it on the
runner's stop timeout), and then call `process::release_proven` until the stop
bound expires. It requires both that the recorded service PID/start identity is
no longer alive and that no inspectable same-UID process holds the clone boot
disk, auxiliary storage or nonce disk by descriptor or fileport. Query errors,
partial scans and missing disk paths are **not** proof. Do not delete the paths
or reuse the slot if proof fails; retain the record and report degraded for
operator attention. A helper lost before reporting a service identity also
remains unproven; the runner's recovery logic must retain its resources.

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

- `macos_qualification_helper_identity`: start two job helpers, prove that each
  reports its own same-`motor`-UID service holding its clone disk, and verify the
  optional responsible-PID answer when available. Confirm that the headless job
  configuration validates, and confirm the exact disk/share/device layout.
- `macos_qualification_helper_sigkill`: SIGKILL a helper with a running VM; verify
  its service exits and releases boot, auxiliary and nonce vnode references
  within two seconds. Prove `release_proven` succeeds only after release. If this
  fails, retain resources/slots and choose an operator-approved fallback before
  rollout; never replace proof with a host-wide VM count or advisory lock.
- `macos_qualification_helper_gate`: kill the runner before opening a recorded
  gate, and again after opening it; confirm no VM starts in the first case and
  the second helper stops its VM before exiting. A failed stop remains degraded.
- `macos_qualification_nonce_channel`: verify in the guest that both raw and
  block nonce devices are root-owned and unreadable by `motorjob` (distinct from
  the host's motor-owned 0400 image file). Confirm correct-Hello authentication,
  refusal of a second connection, and that a bad first Hello fails the job.
  Verify the guest mount setup honors every plan tag,
  guest destination and access mode before any job starts.
