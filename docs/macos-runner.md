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
