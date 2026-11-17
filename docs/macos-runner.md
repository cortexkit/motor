# macOS runner

## Exclusive holds

Set the optional `hold_window` in the VM TOML configuration to a UTC
`HH:MM-HH:MM` window; cross-midnight windows are supported, but equal endpoints
are rejected. Without a window, holds are ignored. The installer accepts
`--hold-window`, defaults new installs to `03:00-05:00`, and preserves the
installed setting on updates.

Write `/Users/Shared/motor-macos-hold/hold.json` with exactly two fields:
`{"owner":"nightly-check","expires_at":"2026-01-01T04:00:00Z"}`. The owner
must contain 1–64 lowercase letters, digits or hyphens, and the expiry must be
RFC 3339 UTC (`Z` or `+00:00`). Holds are honoured only inside the configured
window, with a future expiry no later than that window occurrence's end.
Symlinks, non-regular files, files larger than 4 KiB and malformed holds are
ignored, with invalid content logged once.

While a hold is honoured, new runs receive `RunnerDraining` (callers wait within
their queue-wait budget); already accepted work finishes normally. Once no job
is running or queued, the runner atomically publishes the same two fields to
`/Users/Shared/motor-macos-grant/granted.json`. It removes the grant when the
hold disappears, expires or ceases to be honoured. The installer creates the
hold directory owned by `ghrunner` and the grant directory owned by `motor`,
both mode 0755: each account writes only its own directory and reads the other.
Operator drain/resume remains independent. Health's `admission_blocked` text
reports `held for <owner> until <time> (granted: true/false)` unless a
qualification requirement or operator drain takes precedence.

Portable coverage: `macos::hold::tests` covers window boundaries, midnight,
malformed files and grant removal. `tests::platform_refusal::macos_hold` includes
`honoured_hold_refuses_new_run_while_running_job_finishes`,
`queued_job_accepted_before_hold_runs_before_grant`,
`deleting_hold_reopens_admission_but_never_clears_operator_drain` and
`restart_rereads_hold_and_removes_stale_grant_when_hold_ends`.
The installer's `test_hold_directories_have_separate_writers_and_public_read_modes`
and `test_hold_window_is_optional_validated_and_passed_to_vm_config` cover the
folder permissions and setting pass-through.

The macOS host-preparation API stages snapshots and resolves dependencies before
a job can be admitted to a VM. A preparation refusal is a refusal before start:
the job command has not run.

## Host preparation

Unchanged source files keep their modification times between jobs of the same
checkout, so Cargo can reuse the checkout's build disk (see
[Build disk and `target/`](#build-disk-and-target)). Reuse applies only to files
sealed read-only in both jobs; changed files receive newer times.

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
profile, version 3. Version 3 agents mount the job's build disk at a fixed guest
path outside the shared folders (see [Build disk and `target/`](#build-disk-and-target));
the runner refuses any other version at Hello with a `guest_agent_version` error
that names the image, so a version 2 agent, which mounted the disk over the shared
`target/` directory, never runs a job. The Windows profile is versioned separately.

### Nonce disk and connection

The host attaches exactly three disks: the boot disk, a read-only raw nonce disk
and the writable build disk. At byte zero the nonce disk contains this header,
nonce and bounded map:

```text
MOTOR-NONCE-V2\n
<32 lowercase ASCII hexadecimal characters>\n
<4-byte big-endian length><JSON mount map>
```

Here `\n` means one newline byte, not two literal characters. The rest of that
first 69,632-byte block is zero padding. The JSON array has at most
64 entries and is at most 64 KiB: `[{"tag":"workspace","destination":"/Users/motor/repo","read_only":false}, ...]`.
The nonce is generated per job. There is no v1 reader or compatibility mode.

The build extension follows at byte 69,632 and fills a fixed 1,638,400-byte block:

```text
MOTOR-BUILD-V1\n
<4-byte big-endian length><JSON build configuration>
```

The JSON object is `{"returns":["debug/app", ...],"return_max_bytes":N}`, with
no other fields: the requested returned files inside `target/`, relative to it,
and the caller's byte cap (see [returned files](#returned-files-under-target)).
At most 64 paths, each at most 4096 bytes with no empty, `.` or `..` component.
The whole disk is 1,708,032 bytes. A wrong header, length, field, path or any
non-zero padding byte refuses the boot configuration; the extension is never
partly read. An agent older than the extension reads only the first block, so
it still boots and reaches Hello, where the runner refuses its version.
The guest reads the entire image, both blocks, into a 4096-byte-aligned buffer with one
sector-aligned read before parsing it; neither discovery nor the explicit-device
path probes a short header on a raw device. Every padding byte is checked.
The guest probes whole raw
disk nodes named `/dev/rdiskN` (decimal N, no partition suffix) and requires
exactly one matching magic header; it does not assume a disk number. A matching
header with invalid nonce bytes is an error. The raw and corresponding block
devices must not be readable by `motorjob`; the image must give that account an
unprivileged primary group, with no disk-access groups.

VM boot and the agent's first Hello share a 300-second limit; exceeding it
refuses the job with `workspace_setup_failed` and names the setup phase that
timed out. The job never ran, so resubmitting it is safe.

The root agent connects to `VMADDR_CID_HOST` (2), vsock port **5000**, and sends
`Hello { version: 3, nonce }`. The host must already be listening. It accepts
one session, authenticates the nonce and refuses further connections. The wire
sequence is `Hello` → host `TreeEnd { command }` → agent `Started` → zero or more
`Output` → at most one `BuildRelease { released, reason }` → `Exit { code }` or
`Signal { signal }`. There is no `TreeReceived` or host `Start`. The host may
send `Cancel` after `TreeEnd` until terminal status. The agent mounts the build
disk between `TreeEnd` and `Started`, and releases it after the command's output;
the host allows two minutes for `Started`. `BuildRelease` says whether the
release succeeded; `reason` (at most 1 KiB, untrusted text) says why not. The
agent sends it and the terminal status from memory, so a full guest disk cannot
lose them, and it sends the command's real `Exit` or `Signal` whether or not the
release succeeded. The host treats a terminal status without
`BuildRelease { released: true }` as an unreleased disk. Only the macOS profile
accepts `BuildRelease`. When the build disk cannot be mounted, no command runs:
the agent sends `Started`, one standard-error `Output` line giving the reason,
and closes without terminal status (see
[Build disk and `target/`](#build-disk-and-target)).

### Shares and command execution

The workspace is mounted read-write at its caller's absolute path. Each sibling
is mounted read-only at its canonical caller path and, for a linked worktree,
beside the workspace so `../<sibling name>` resolves to the same snapshot.
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

1. The runner emits `workspace`, `cargo-home` and `sibling-N` tags (decimal N).
   Canonical siblings keep indices 0 through k−1; aliases are numbered from k
   onward in sibling order, where k is the number of declared siblings. Aliases
   use the existing sibling role so the agent baked into the sealed guest image
   remains unchanged; no image rebuild is required. The alias destination is the
   workspace caller path's parent joined with the canonical sibling's basename;
   it is omitted only when equal to the canonical path. Both sibling shares
   expose the same staged host directory read-only, so source timestamps are
   applied once to that directory, not separately for the alias.
   `linked_worktree_sibling_alias_shares_the_read_only_snapshot`,
   `adjacent_workspace_sibling_has_no_alias_share` and
   `sibling_alias_map_uses_existing_guest_roles_in_canonical_then_alias_order`
   pin the layout, access rules and compatibility with the unchanged agent.
2. Canonical and alias destinations together may consume at most 62 sibling
   entries, leaving workspace and cargo-home within the 64-entry map bound.
   This permits 31 siblings when every sibling needs an alias, or up to 62 when
   none does; mixed layouts count each required alias. An oversized map is
   refused before tools or staging begin, never by dropping an alias.
   `sibling_aliases_count_toward_the_boot_map_limit_before_preparation` and
   `sibling_aliases_count_toward_the_64_entry_limit` cover the boundary.
3. A canonical or alias destination that equals, contains or lies beneath
   another share destination is refused before preparation, with both paths
   named in the error. Comparisons are case-insensitive, as for the guest's
   mount map; an alias never mounts over another share.
   `sibling_alias_collisions_are_refused_before_preparation` covers workspace,
   canonical sibling and alias conflicts, including nested destinations.

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
`RUSTUP_HOME=/opt/motor/rustup`, `CARGO_HOME=/opt/motor/job/cargo-home`,
`CARGO_TARGET_DIR=/private/var/motor/build` (the build disk's mount), and
`PATH=/opt/motor/cargo/bin:/opt/motor/bun/bin:/opt/motor/bin:/usr/bin:/bin:/usr/sbin:/sbin`.

### Background services in job VMs

The image recipe disables unused system services to leave more memory for builds
and avoid background indexing, downloads and update work. Its pinned policy is
`script/macos-runner/guest-services-off.txt`: Spotlight, analytics and telemetry,
location and Find My, Bluetooth and Wi-Fi (jobs use virtio networking), iCloud
and push notifications, software updates and assets, personal activity history,
power telemetry and MDM. Spotlight indexing is also turned off on all volumes
mounted during provisioning with `mdutil -a -i off`.

System Integrity Protection stays enabled. Provisioning reboots the guest before
sealing so the seal can check persistent launchd overrides and actual running
services, rather than trust a successful disable command. New policy entries
start as `unverified`; a service still running after reboot is recorded as
`kept-sip`, not silently reported as off. Once an entry is pinned `off`, running
again fails the seal. Outcomes are kept in the guest policy file and sealed image
metadata. The seal also requires Spotlight indexing on `/` to be disabled.

The guest agent, security and permission checks, DNS, file-system mounting,
logging and crash reporting remain enabled. User-session services such as Siri,
suggestions and user Spotlight workers are not disabled without evidence of a
running user domain; job VMs have no GUI login. Services without a confirmed
system launchd label are left alone. Volumes mounted later need their own
mount-time Spotlight exclusion; the provisioning command does not set that
policy for future build disks or workspace shares.

### Build disk and `target/`

The job's build output is not on the shared folder. On that share, listing a
directory while entries in it are deleted can skip real files, so `cargo clean`
left files behind and rustc's incremental garbage collection failed with
"Directory not empty". Each checkout instead keeps its build folder on its own
disk, which the job VM formats and mounts as APFS at the fixed guest path
**`/private/var/motor/build`**. Jobs get that path as `CARGO_TARGET_DIR`, and
the checkout's `target` is a link to it, so `target/...` paths reach the same
volume.

The variable applies to every Cargo build in the job, not only the checkout's.
A job that builds several source trees (copies of the checkout at other
commits, for example) must give each tree its own `--target-dir` or
`CARGO_TARGET_DIR`: in one shared folder Cargo reuses one tree's build-script
output for another, so a binary can report the wrong tree's commit. Linux jobs
behave the same way with their `/run/motor/target`.

**Why not mount it at `target/`.** An earlier version mounted the volume over
the shared `target/` directory itself. In the guest, a path through a
virtiofs directory stopped crossing a mount on it about 3.5 seconds after
mounting, whatever the job did: the volume stayed mounted, but `target/`
named the shared directory again and builds wrote to the shared folder. The
volume is therefore never mounted on a share. `CARGO_TARGET_DIR` is set, as
Linux jobs set it, so Cargo builds on the volume even if a job removes or
replaces the `target` link; earlier versions promised it stayed unset, which
only held while the volume was mounted at `target/` itself.

- **On the host**, the runner keeps one file per checkout, `targets/<key>.img`
  under its state root: a sparse raw disk of exactly **128 GiB**, of which only
  written blocks use host disk. For each job it makes an APFS clone of that
  file, a constant-time copy, in the job's clone directory, and attaches it to
  the VM as a writable USB mass storage device, with the boot disk's caching
  and synchronization settings. A checkout's first job gets a new, empty disk.
  The staged checkout's `target` is replaced by a link to
  `/private/var/motor/build`. The host never follows that link: on the host
  its target names an unrelated path. Source timestamps skip snapshot files
  under `target`, and after the VM has stopped the runner removes any link at
  `target` before scanning the checkout for changes.
- **Why USB.** On Apple silicon, macOS protects the key of every APFS volume
  on internal storage with the Secure Enclave. The guest treats a virtio block
  disk as internal storage, and a job VM's Secure Enclave state belongs to its
  clone of the image, which is discarded after the job. A volume formatted on
  a virtio disk is mounted with the `protect` flag and does not mount in the
  next job VM: `mount_apfs` fails with "Illegal byte sequence", so every build
  started cold. Removable storage does not use the Secure Enclave, and the
  guest treats a USB mass storage device as removable.
  `script/macos-runner/build-disk-probe.sh` checks this on a host without a
  runner: for each attachment it boots two job VMs from clones of a sealed
  image and reports whether the second mounts the volume the first formatted.
  Each VM builds a small Cargo package with a returned file under `target/`
  and runs for more than 10 seconds, and the probe reports any step at which
  `target/` or `CARGO_TARGET_DIR` no longer reached the volume.
- **In the VM**, before the command starts, the root agent requires the
  checkout's `target` to be exactly the runner's link. It prepares
  `/private/var/motor/build`: every directory on that path is opened without
  following links and must not be owned or writable by `motorjob`, missing
  ones are created as root with mode 0755, and the mount point must be an
  empty directory with nothing mounted on it. It finds the disk by its fixed
  size, the one property the guest cannot change and no other disk shares.
  It formats the disk as APFS only while it is still blank (all zero, as the
  host creates it). It then mounts the volume `nosuid,nodev` and gives the
  volume's root to `motorjob`, clearing any user flags such as `uchg` an
  earlier job set on it. The raw and block devices must not be writable by
  `motorjob`. A workspace below a directory `motorjob` owns or can write, its
  home directory included, or below a link also refuses the job. If an
  existing volume is missing, will not mount or cannot be handed to
  `motorjob`, the agent reformats the disk, and the job's standard error
  starts with a line saying so: that build starts cold. The agent never runs
  the command without the volume mounted.
- **While the command runs**, the agent looks at the mount every 100 ms, and
  at `target` without following it (whether it is still the runner's link;
  as root it never resolves a link the job may have changed). Just before
  releasing the volume it adds one line to the job's standard error,
  `motor: build-volume diagnostic watch: ...`, saying whether both held and,
  if not, when that stopped (milliseconds after mounting and after the
  command started) with the last good look. Lines with this prefix arrive on
  the job's standard error, where the job can write them too, so the runner
  logs them as job output with the job ID, at most eight per job.
- **After the command**, once every `motorjob` process is gone, the agent
  reaches the mount again from `/`, one directory at a time without following
  links, and requires the volume root it recorded before the command (the
  same file system ID and inode). It copies the returned files out (below),
  then unmounts the volume, which must have exactly one mount, forcing the
  unmount if a killed process left it busy. It then counts `motorjob`
  processes again, after the unmount and after placing the returned files:
  any process alive then fails the release. It sends
  `BuildRelease { released: true }`, then `Exit` or `Signal`. If any of this
  fails, it sends `BuildRelease { released: false, reason }` and the
  command's own `Exit` or `Signal`, so the caller gets the command's real
  status; the caller's standard error also ends with
  `motor: the build disk could not be released: <reason>` when the agent can
  still write it. The runner logs the reason with the job ID.
- **Publication.** After the VM has provably stopped, the runner renames the
  job's disk over `targets/<key>.img` when the agent sent
  `BuildRelease { released: true }`, because only then was the volume cleanly
  unmounted, and the checkout's `target` is still exactly the runner's link. **A failed job publishes its
  disk too**: a non-zero exit, such as a compile error, or a command killed by
  a signal keeps its build output for the next job, as a Linux workspace keeps
  its `target/`. A cancelled job, a release failure, a job that removed or
  replaced the `target` link, a session that ended without terminal status,
  or an unproven stop publishes nothing; the next job uses the previous disk,
  and the job's exit stands. The job's standard error never decides it. The
  rename is the commit point: a cancel that
  arrives first stops it, and one that arrives during it waits until it is
  done and finds the disk published. The host never mounts or reads the disk's contents: it only
  creates, clones, renames, measures and deletes the file.
- **A full disk.** The volume holds at most 128 GiB. A build that fills it fails
  like one on a full disk, with "No space left on device" (`ENOSPC`) from the
  compiler or linker. Deleting files in `CARGO_TARGET_DIR` in a job frees the
  space for the checkout. The mount point itself cannot be removed, so a tool
  that deletes the whole directory, as `cargo clean` does, reports an error
  after emptying it.
- **Eviction.** Below 40 GiB free on the state volume, admission-time cleanup
  deletes build disks least recently used first, measured by the host disk they
  take, not their 128 GiB apparent size; a disk whose checkout has a job in
  progress is kept. See [scheduling](scheduling.md).
- **Migration.** A build folder kept by an older runner (`targets/<key>`) is
  never converted. The checkout's first job on a build disk builds cold, and the
  old folder is deleted when that job's disk is published. Eviction deletes old
  folders before any disk.

#### Returned files under `target/`

The host cannot read the volume, so the agent copies requested returned files
that name something under `target/` before it unmounts it. A return path
counts as under `target/` when, relative to the checkout, its first component
is `target` (in any ASCII case) and every other component is a plain name. In
request order, a file is copied while its size still fits under the caller's
cap, the same rule the host applies, into a private root-only folder on the
guest's own disk. After unmounting, the agent creates
`.motor-target-returns` in the job's cargo-home share, which is never shared
with another job and is deleted with the job, and writes the copies there at
their paths relative to `target/`. That directory must not exist yet: the job
can write the share, so if it does, the agent places nothing and reports a
release failure. The host reads a path `target/<rest>` from
`.motor-target-returns/<rest>`, with the same no-follow walk confined to that
directory as for any other returned file, and only when the build disk was
released and the checkout's `target` is still the runner's link; otherwise
those paths are reported unread. Any other path that reaches the checkout's
`target`, through `..` or a link, is reported unread: the host never follows
the link. Return paths outside `target/` are unchanged. A file over the cap is
written as a sparse placeholder with none of its bytes, sized one byte over
the whole cap (or the file's own size, if larger). The host budgets every
returned path, the agent only those under `target/`, so a placeholder of the
file's real size could fit the host's remaining budget and come back as
zeros; one larger than the cap never fits, and the host reports it over the
cap. The agent never follows a link on the volume or crosses onto another file
system mounted below it: a returned path under `target/` must name a regular
file on the build volume through real directories, or the host reports it
missing. Each placed file, and the directory holding it, is flushed before the
release is reported; a file that cannot be written whole is removed, so the
host reports it missing rather than returning part of it, and if it cannot be
removed the release fails. Before creating `.motor-target-returns` the agent
requires the cargo-home share to be the one it opened before the job (the same
file system ID and directory inode), and it opens that share only when no
directory above it is `motorjob`'s or writable by `motorjob`. A path whose
first component is `target` in any ASCII case is the runner's everywhere:
source sealing and source timestamps leave it alone too.

The image recipe installs Rust 1.91.0, 1.92.0, 1.96.0, 1.99.0 and 1.100.0, plus
an independent stable copy of 1.99.0. The current macOS job image still defaults
to 1.99.0 until rebuilt; Linux jobs default to 1.100.0. Parity allows a macOS
default that is also installed on the build server, while still checking the
job's observed default against the sealed macOS manifest. Rust 1.100.0
has the same components and `aarch64-apple-darwin` target as 1.99.0, including
`llvm-tools` and `rust-analyzer`. Bun is 1.4.4. CMake 4.2.3 uses Kitware's
SHA-256-pinned official macOS universal archive; its CLI binaries and module tree
are read-only under `/opt/motor/cmake`, with `cmake`, `ctest` and `cpack` links in
`/opt/motor/bin` on the job PATH. The image measurement records exact tool
versions, Rust component/target sets and native executable/toolchain hashes.

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

The host installer runs the runner as a LaunchAgent in the dedicated non-admin
`motor` account's GUI login session, not a system LaunchDaemon. Apple's
Virtualization framework needs an Aqua session to create its Secure Enclave key
at VM start; outside one, it can fail with `errSecInteractionNotAllowed` (-25308)
and “The virtual machine encountered a security error.” Merely setting a system
job's `UserName` does not provide that session.

The installer requires FileVault off and makes motor the auto-login user. A
random account password is saved root-only at `/var/root/motor-autologin-password`
(0600). It writes the NUL-terminated, 12-byte-padded Apple XOR encoding to
`/etc/kcpassword` (root-owned 0600) and sets loginwindow's `autoLoginUser`. This is
obfuscation, not encryption; automatic login permits physical/console access to
the dedicated account after restart. Previous login settings and the original
login keychain are backed up root-only in
`/var/root/motor-autologin-backup.plist`. The login keychain is recreated with the
new password and selected as motor's default once; updates preserve it and reuse
the password. There is no keychain-unlock workaround.

The root-owned 0644 plist is
`/Users/motor/Library/LaunchAgents/io.cortexkit.motor.macos.plist`, in a directory
owned by motor. It sets `LimitLoadToSessionType = Aqua`, without `UserName` or
`GroupName`. Installation removes the old system LaunchDaemon and uses
`launchctl bootstrap gui/<uid>` / `bootout gui/<uid>/io.cortexkit.motor.macos`.
Without motor's GUI session it waits for the next login. A changed auto-login
user requires an operator restart; the installer never reboots the host.
`--check` verifies the agent configuration, auto-login user, FileVault-off status,
and that the job is running in `gui/<uid>`; no Aqua session is a clear restart
refusal, not a skipped check. `InstallerTests` and `AutoLoginTests` in
`script/macos-runner/test_macos_runner.py` cover this lifecycle hermetically.

Uninstall stops/removes both runner jobs/plists, restores the backed-up
auto-login settings and login keychain/default, removes the generated password
and backup, and removes only the managed SSH block. It revokes the saved motor
credential by rotating to a fresh random password and discarding it; the account
remains enabled, and the unknown prior password is not recovered. If the previous
auto-login user was motor, uninstall instead clears auto-login and removes
`/etc/kcpassword`, avoiding auto-login with an invalid credential; the operator
can re-enable auto-login for their own account. Account, image and runner
state remain; restart afterwards to leave the login session. See the
[installation procedure](../script/macos-runner/README.md#host-installation-and-update)
for commands and the console-access tradeoff.

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
projection also uses explicit heap stacks with that depth limit; the input-copy
buffer is one heap allocation, never a recursive frame.
Unremovable/deep trees are moved under
the job's recorded `quarantine/<job_id>` path (or remain at the recorded original
path if relocation fails). Their inventory remains for retry/operator attention,
but they hold neither a VM slot nor the workspace lock. Quarantine remains visible
as degraded status and a warning; direct runner admission can still use free slots.
Recovery ignores unrelated/temp record-directory entries, while valid ownership
records with invalid resource paths still fail closed.

`motor-runner drain --instance macos --state-root <path>` persists its admission
hold as `macvm-drained` in the state root across runner restarts. Running jobs
finish, but new jobs receive the existing typed never-started `runner_draining`
refusal until `motor-runner resume --instance macos --state-root <path>` clears
the hold. A launchd restart does not reopen admission. If the marker cannot be
inspected at startup, admission stays closed and the runner logs why. The status
reply's health field reports `admission_blocked: "runner_draining"` while closed.
If operator qualification is also required, health reports
`runner awaiting qualification` instead; resume does not clear that separate
hold. A failed marker removal fails resume without reopening admission.

The five job-level ignored `macos_qualification_*` integration tests run on the VM host
against the **installed** Aqua-session LaunchAgent through `ck motor run --platform macos`,
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
It also creates a blank build disk, `build-disk.img`, in the clone; the runner
replaces it with a clone of the checkout's kept disk when there is one.
The runner must record the clone and nonce paths durably **before** creating them;
creation failures intentionally leave any partial resource for recorded cleanup.
`create_job_nonce_disk` exclusively creates the 1,708,032-byte, owner-readable,
read-only raw image with the nonce format and build extension above, zero
padding, and fsyncs it and its parent.

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
disk, auxiliary storage, nonce disk or build disk by descriptor or fileport. Query errors,
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

The build disk is untrusted guest output and is published after failed jobs as
well as successful ones, including signals, once the agent reported terminal
status and VM disk release is proven. This deliberately matches Linux, where a
workspace retains its target between jobs. The case-insensitive workspace key is
not a branch/principal integrity boundary: later jobs may consume earlier build
artifacts. The host only creates, clones, renames and removes that file; it never
mounts, executes or parses it. The guest kernel mounts a volume that an earlier
job of the checkout wrote through ordinary file operations only, since `motorjob`
cannot write the raw device. Each checkout's disk is capped at 128 GiB; there is
no cache integrity partitioning.

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
