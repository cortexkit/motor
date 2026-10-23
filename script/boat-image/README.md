# Boat runner host recipe

Apply as root in a fresh **Ubuntu 24.04 x86_64 Boat `large`**, created with
`noEnv: true`. Do not attach account secrets, credentials, repositories or CLI
logins. The explicitly acknowledged fleet cache below is the only private source
input. Other downloads are public; the recipe never upgrades/replaces the
provider's init or sshd binaries. The source checkout must be clean and at the
exact revision being installed:

```sh
sudo /usr/bin/python3 script/boat-image/install.py --revision "$(git rev-parse HEAD)" \
  --cargo-seed-manifest /secure-input/cargo-seed.json \
  --cargo-seed-archive /secure-input/cargo-seed.tar.gz \
  --cargo-seed-sha256 "$CARGO_SEED_SHA256" --acknowledge-tenant-one-private-cache
sudo systemctl start motor-boat motor-boat-identity
```

`pins.json` fixes the official signed Ubuntu source URLs and hashes the shared
parity manifest, checker, Cargo-tool installer and sccache build inputs.
`script/runner/toolchain.json` is the single job-tool parity manifest: Rust/rustup,
per-toolchain targets/components, Cargo binaries and install sources, required
packages, Node/npm, Bun, pnpm and NATS. Release archives use publishers' SHA-256
lists; pnpm uses the exact version's published SHA-512 integrity. Rustup and
Cargo verify distribution/crate checksums; git tools use the full revisions in
the parity manifest and locked dependencies. The patched sccache is built
with `script/runner/build-sccache.sh`; its read-only-cache patch is required
because stock sccache touches cache files even for a hit. Anonymous access to
`github.com/cortexkit/commons` is tested inside Boat before installing its pinned
`ckdev-mutate` mutation-testing CLI revisions.
No credential is forwarded if a public fetch fails. The resulting inventory
records versions, package resolution, runner source revision and binary hash.
The Ubuntu snapshot backend was unavailable, so rebuilds use the signed live official archive and record every installed package version plus all three Release dates; job-to-job reproducibility comes from the immutable named Boat snapshot, not from rebuilding the same OS packages later.
APT uses an isolated source/list directory containing only archive.ubuntu.com and
security.ubuntu.com, with Ubuntu's archive keyring and normal signature checks.
Every index refresh uses `APT::Update::Error-Mode=any`; transient 5xx errors get
at most three attempts with bounded backoff, and any remaining error aborts
before package installation. The guest preflight records those same three official
indexes, separately from APT's actual signature verification.
These are reproducible tool/source pins, not a claim that independently linked Rust
binaries have identical hashes. Reapplying a successful install reuses the disk,
accounts, pinned Cargo tools and patched sccache build marker.

## Checked tool parity

### Fleet-only offline Cargo seed

This recipe's Cargo seed is an **operator-supplied, tenant-1-only input**. Git
objects can contain private fleet source. Never publish the seed, embed it in a
generic/customer image, or attach this prepared image to another tenant. The
recipe requires explicit acknowledgement; no seed is committed to this repository
or downloaded implicitly. Archive and manifest exports are created mode 0600 and
refuse to overwrite an existing file.

Capture only the server's shared Cargo home: jobs mount its registry and Git
trees read-only, and only trusted dependency prefetch writes them. Do not export
a job's private Cargo home. Capture is read-only, but a concurrent trusted fetch
can invalidate the inventory. Export rehashes each inventoried file before
archiving it; import verifies the supplied manifest and archive members, not the
original server tree. Prefer a quiet prefetch interval.

```sh
umask 077
mkdir -p /secure-output
python3 script/runner/cargo-seed.py export \
  --cargo-home /srv/build/cargo-home \
  --manifest /secure-output/cargo-seed.json \
  --archive /secure-output/cargo-seed.tar.gz --acknowledge-tenant-one-private-cache
```

Record the printed `manifest_sha256` independently; transfer the two files over
an authenticated operator channel to the builder. The sandbox checks that digest,
the archive digest, **every member's hash**, and each crate's sparse-index checksum
before using any payload. Export includes registry index metadata, `.crate`
archives and Git object databases/refs, but not Cargo credentials/configuration,
Git configuration/hooks, registry sources or Git checkouts. The public registry
`config.json` is index metadata and is retained. External Git object alternates,
symlinks, nonregular archive members and path traversal are refused. Sparse
registry metadata must exist for every cached crate; an unsupported legacy index
fails explicitly instead of producing an incomplete seed.

Sources are unpacked from verified `.crate` files during installation because
jobs cannot unpack into the read-only shared registry. They save **transfer**
space, not final source-tree space. Git checkouts for locked dependencies are
regenerated by trusted prefetch. Export reports exact archive/payload bytes and
elapsed seconds; `inventory.json.cargo_seed` records capture time, crate count,
archive/payload/source/installed bytes and import seconds. Transfer time and
the rest of the image build remain visible in the probe's upload/install timings.

The seed is a point-in-time floor, not a promise of future cache completeness.
The root manifest's locked fetch remains mandatory before offline execution.
Nested tracked lockfiles are **best-effort cache warming**, not job-admission
requirements: stale experimental crates must not prevent unrelated jobs from
starting. Within a 128-lock snapshot budget (including the root), each eligible
nested manifest gets its own 30-second trusted fetch, with at most one additional
second for forced teardown through GNU `/usr/bin/timeout`. A failed fetch,
missing sibling manifest, nonregular pair or over-budget lock is skipped with a
setup warning containing the job ID, lockfile and error. Untracked nested locks
are ignored. A job that later builds an unavailable nested graph fails with
Cargo's own offline error; refresh the seed or fix and commit that graph's lock.
`stale_nested_lock_does_not_refuse_and_root_still_fetches`,
`stale_root_lock_still_refuses_before_nested_fetch`, and
`nested_lockfile_prefetch_populates_offline_sources` defend these boundaries.

Parity compares fresh files with the operator's server seed inventory, not the
image's saved receipt. Extra image crates are allowed. Missing crates are reported
as a count and up to ten name/version examples; corrupt archives, missing index
metadata/configuration and missing Git database files also fail. A newer server
capture can be checked against an older image without rebuilding it first:

```sh
python3 script/runner/toolchain-parity.py --check --role boat \
  --cargo-seed-manifest /secure-input/cargo-seed.json \
  --cargo-seed-sha256 "$CARGO_SEED_SHA256"
```

`script/runner/cargo-offline-probe.py` runs inside a real offline job in each
qualification phase. It checks a nested helper with a lockfile using
`cortexkit-cow = 0.1.0` and a fresh manifest with no lockfile using
`cortexkit-bus-testkit = 0.1.3`. Both use `cargo check --offline`; no network or
job-side tool repair is allowed. Portable tests use synthetic crates with those
names and real Cargo, so they require no public service.

All runner-host probe commands below additionally require
`--cargo-seed-manifest /secure-input/cargo-seed.json --cargo-seed-sha256 "$CARGO_SEED_SHA256"`.
They also require `--acknowledge-tenant-one-private-cache`.
Recipe application also requires `--cargo-seed-archive /secure-input/cargo-seed.tar.gz`;
`--verify-prepared-image` needs only the manifest and independent digest. Probe
uploads are over pinned SSH; private archives are removed before snapshotting.

### Toolchain inventory

Capture the build server without installing or changing anything, then compare
its live state with the reviewed manifest:

```sh
python3 script/runner/toolchain-parity.py --capture > /tmp/toolchain-state.json
python3 script/runner/toolchain-parity.py --check \
  --cargo-seed-manifest /secure-output/cargo-seed.json \
  --cargo-seed-sha256 "$CARGO_SEED_SHA256"
```

To propose a manifest refresh, run the following from the source checkout.
It writes JSON only to stdout; review the result before replacing the manifest
and refresh the `runner_inputs_sha256` entries in `script/boat-image/pins.json`
and `script/tenant-image/versions.txt` afterward:

```sh
python3 script/runner/toolchain-parity.py --from-capture /tmp/toolchain-state.json \
  > /tmp/toolchain-proposed.json
```

The recipe installs every listed target and component **before** jobs are
admitted, removes unlisted toolchains/components on a reused image, selects
MinGW's win32 thread model, and provides the server's `gnu*` command names using
Boat's GNU coreutils. Bun lives under `/home/ubuntu/.bun/bin`; the runner's HOME
selects that read-only job bind, preserving the server's job PATH order.
Rustup remains read-only in jobs; no job-side repair or tool installation is used.
`rust-analyzer`, `rls` and `cargo-miri` are proxies, not installed components in
the measured contract. Boat never installs the moving `stable` channel. It
copies the pinned numbered release matching the server's captured stable compiler
into the `stable` toolchain directory, then removes unlisted components from that
independent copy. Rustup rejects official names such as `stable` for `toolchain
link`; copying also preserves the server's smaller stable component set without
removing clippy/rustfmt from the numbered release. No component superset exception
is used, and the compiler-version check stays strict.

Installation runs the parity check. Qualification reruns the **operator
checkout's** checker against live files, not a saved inventory or the image's
own checker, in each initial/clone/resume phase. A real Rust job additionally
checks executable paths/versions, PATH order, wrapper/cache paths and the rustup
mount's read-only flag. Missing targets, components, Cargo binaries or required
packages prevent a PASS verdict, including in `--verify-prepared-image` mode.
The checker reads rustup's installed lists and verifies each target's `libstd`
payload exists; a stale installation receipt alone cannot qualify a target.
GNU aliases are identified by the owning `coreutils`/`gnu-coreutils` package.
When no package owner is reported, the checker accepts version output containing
either `(coreutils)` or `(GNU coreutils)`.
`gnutest` has no version flag: its host package ownership is checked without
executing it. The probe compares the job's `gnutest` file hash with a fresh hash
of the host's `gnutest` file because job filesystems omit dpkg's database.
The report carries the manifest digest and observed versions.

Everything root executes from must be root-owned with no group or other
write. That covers every entry under `/usr/local`, `/usr/bin`, `/usr/sbin`,
`/bin`, `/sbin`, `/srv/build/cargo-home/bin`, `/home/ubuntu/.bun/bin` and
`/home/ubuntu/.rustup`, plus each directory above them (the PATH and
RUSTUP_HOME of `motor-boat.service`). Symlinks must be root-owned too.
Installation enforces this after the service preflight, recorded as
`root_owned_paths` in the inventory, and every probe phase reruns the operator
checkout's `script/tenant-image/guest/root_paths.py` as `<phase>.root_owned_paths`.
Boat's base image ships `/usr/local` owned by uid 1000, its sandbox account
(Boat's exec API runs as that account, which has passwordless sudo). Right
after installing packages, before the tool installers write under `/usr/local`, the recipe
applies `BOAT_BASE_USR_LOCAL` in `install.py`, recorded as `boat_base_layout` in
the inventory. Ubuntu's skeleton (`/usr/local` and the base-files and package
directories named there) stays and is re-owned to root:root with group/other
write cleared, one path at a time, never recursively. Every other entry under
`/usr/local` that uid 1000 owns, at any depth, is removed: Boat's Go, rustup,
Cargo and Ruby installs, none of which the image uses (the runner, endpoint and
jobs take Rust from `/home/ubuntu/.rustup` and `/srv/build/cargo-home`, and the
build server's manifest names no Go or Ruby), together with any symlink directly
in a PATH directory that points into a removed entry. The walk never follows a
symlink. The inventory records the removals as counts per path two levels below
`/usr/local` (`/usr/local/include/ruby-4.0.0: 5`, say). An entry owned by any uid
other than 0 or 1000 refuses the build before the step changes anything, as does
a removal that would take the recipe's own interpreter, standard library or
source checkout, which is why the recipe runs as `/usr/bin/python3`. Root-owned
entries stay where they are, except inside a removed provider directory or as a
PATH symlink into one; a remaining root-owned entry that is unsafe still fails
the root-owned path check by name.
Before installing the runner toolchains, the recipe creates an absent `/home/ubuntu` as
root:root 0755. If another account owns it, the recipe refuses and names the
owner's uid, passwd entry and running processes; it never takes over a
provider account's home. Release archives are extracted with `--no-same-owner
--no-same-permissions` under umask 022, because Node's tarball records uid 1001.
The manifest separates compiler entry points by role: the build server uses
`/motor-tools/prefrontal-build-slot`, whose `.cache` must name the trusted
`/motor-tools/sccache` script; single-job Boat uses `RUSTC_WRAPPER=sccache`
directly. Both roles check the wrapper's source hash, backend path and read-only
mounts for `/motor-tools` and the shared Cargo `bin` directory. All other job
environment values use the manifest's common contract.
`test_recipe_preinstalls_wasm32_unknown_unknown`,
`test_missing_image_wasm_target_fails_check`, and the probe's parity tests defend
the missing-target regression and fresh-measurement boundary.

The manifest explicitly lists accepted OS-level differences rather than claiming
identical Linux distributions. Required distro packages and GNU aliases must
exist, but their versions are recorded rather than equal across Ubuntu releases.
Binaryen is not an accepted difference: Boat and tenant remove the distro package
and install the official Binaryen 120 x86_64 Linux release from GitHub. Its archive
URL and SHA-256 (`ddb097af51d1bdb17300d986b0de7d97422f1933dedb4c9eda3510e0bf4076cc`)
are pinned in `toolchain.json`; `/usr/local/bin/wasm-opt` resolves into
`/usr/local/lib/binaryen/120`, ahead of `/usr/bin`. Parity requires major
version 120, matching the build server. Each real Boat job also runs
that binary on a tiny valid WebAssembly module and optimizes the output again;
the probe uses a round trip rather than a hand-encoded newer proposal module.
ENGRAM provides esbuild through its own nested JS lockfile install, not a global
esbuild. The runner supplies neither `ESBUILD_BIN` nor the `WASM_*_BIN` overrides;
repositories set those themselves. Native Cargo executable hashes can vary across
builds; Cargo versions/default aliases are exact, while Boat's patched sccache is
checked using its build-input marker, recorded output hashes and a distinct stock
binary. The server's sccache hashes are checked directly. Inventory never starts
either sccache executable.

Git is not an accepted difference. Ubuntu 24.04's git is 2.43, and git's
behaviour changes between that and the server's 2.53: from 2.46 an empty
`remote.<name>.pushurl` or `remote.<name>.url` value clears the URLs set by
earlier config, where 2.43 keeps them and adds the empty string as another URL.
`script/runner/build-git.sh` builds the server's release from the kernel.org
source pinned in `toolchain.json` (`tools.git`: URL, SHA-256, install
directory), with libcurl (GnuTLS) for https, expat for http push, PCRE2 and
gettext as in Ubuntu's package, into `/usr/local/lib/git/<version>`, and links
`git`, `scalar` and the server-side commands from `/usr/local/bin`. Ubuntu's
git package stays installed at `/usr/bin/git` for packages that depend on it;
the runner's unit PATH and the job PATH both search `/usr/local/bin` before
`/usr/bin`.
Parity checks Boat's `git` path, resolved executable and exact `git --version`
against the server's capture. A server git upgrade refreshes the manifest's
version through `--from-capture`; `validate` then refuses the manifest until
the source URL and install paths name the new release, and a SHA-256 left from
the old release fails `build-git.sh`'s download check.

Each qualification phase also runs `script/runner/git-pushurl-probe.py` inside
a real job: it resets an inherited pushurl at `--worktree` scope with an empty
value, adds one URL, and requires `git remote get-url --push --all` to print
exactly that URL. It fails with git before 2.46.

## Filesystem choice

Boat's measured ext4 root rejects FICLONE (`EOPNOTSUPP`). Use a **64 GiB sparse,
loop-mounted XFS image** on that disk, with `reflink=1`, not a plain-copy runner
fallback. The state, warm workspaces, JS install caches and `/Users` workspace
bind all share that filesystem. The recipe actually clones random nonempty data
between the state mount and workspace bind, and checks the bytes. A loop-image
or ioctl failure aborts installation rather than changing copy semantics.

Both mount points must be empty on the root disk in the image. After hydration,
`motor-boat-storage.service` checks the current views before mounting anything;
non-empty directories cause a logged refusal, never automatic deletion or a
mount hiding existing data. The restore gate waits for Boat to retire its public
ext4 binds at both paths and for five seconds of stable real-disk observations;
the early restore marker alone does not cover deferred mount cleanup. Boat's
private bind mounts under `/var/lib/ascii-lazy/sysbind` remain owned by Boat's
restore agent; Motor does not unmount them. Storage preparation reloads the
restored unit definitions, starts the XFS mount unit, creates the workspace
directory on that mount, then starts `Users.mount`. Only systemd mounts these paths.
Neither mount unit is enabled at boot or depends back on storage preparation.
Once storage evidence is published, a condition on both units blocks a fresh
activation from mounting over live state. Re-establishment requires stopping the
runner, endpoint and storage watcher before restarting storage preparation.
Each runner
and endpoint start independently rejects duplicate mount targets and checks the exact XFS
mount, its loop device's backing file `/var/lib/motor-boat.xfs`, the `/Users`
bind identity, `reflink=1`, and a real cross-path FICLONE. An active mount unit
alone is not evidence of usable storage.

Before each Boat job launch the runner also checks the workspace exists on the
same non-root device as `/Users` and runner state. The storage watcher checks
device and directory identities immediately, then sleeps one second after each
check. Filesystem operations and scheduling can extend that interval; it is not
a hard real-time deadline. Before reading evidence or checking storage, every
watcher instance attempts to withdraw readiness. Any unhealthy result or exception
attempts to revoke health and remove the marker before logging, then exits nonzero.
A failed unlink is fatal, with best-effort cleanup retried on exit. If cleanup
cannot complete, the endpoint rejects the health lease after it expires.
Only a new watcher instance that
re-verifies the prepared identity can report healthy again; the watcher never
creates endpoint readiness or repairs mounts beneath a running daemon.

The endpoint is the sole readiness publisher. Its key/identity checks alone used
to recreate a marker removed by the watcher on the next endpoint poll. Both now
take an exclusive `flock` on `/run/motor-boat-storage/lock` across publication or
withdrawal. The watcher alone writes root-owned 0600 `health.json` in that root-only
directory, containing status, the verified identity tuple and a five-second
monotonic lease. The endpoint publishes only for a healthy, unexpired lease whose
tuple matches the state/workspace identity it captured at startup. Missing,
unreadable, malformed or unsafe health files fail closed. A queued publisher reads
health after taking the lock, so it cannot resurrect a withdrawn marker.

The notify service uses `WatchdogSec=5`, with heartbeats only after completed
polls. `READY=1` starts the watchdog before evidence reads; it reports monitor
liveness, not permission to publish readiness. `ExecStopPost` also revokes health
and withdraws readiness after uncatchable exits. The health lease expires even
if crash cleanup cannot execute. The lock directory survives service restarts
and endpoint runtime-directory removal, preventing replacement of the shared
lock inode. Restart attempts are limited to five starts per 60 seconds; repeated
failure ends in a failed unit, not an unlimited loop. Cleanup withdraws readiness;
if cleanup itself fails, the endpoint withdraws on its next poll after lease expiry.

Portable tests cover delayed public-bind
cleanup, systemd-only activation, live-consumer refusal, and readiness withdrawal;
the real runner-host probe additionally observes the first twelve seconds after
readiness, sequential jobs, a seven-minute idle/reclaim interval, and stop/resume.
If the provider's counters do not prove a reclaim during that interval despite
a successful service run, the report marks the idle-reclaim check **INCONCLUSIVE**,
not `PASS` and not a Motor failure. That observation gap does not block
job parity qualification; parity and storage failures remain blocking.
At the end, on the disposable clone only, it unmounts `/Users` as deferred cleanup
would, creates a root-disk workspace sentinel, and requests `Users.mount` again.
The sentinel must remain visible without an overmount, and endpoint readiness
must disappear within **three seconds**, allowing three nominal watcher polls.
The replay samples every 100 ms for up to ten seconds, recording
`withdrawal_latency_seconds` as the first observed absence, or `"never"`.
Timing starts immediately before `umount`, so the reported latency includes
the unmount command and is an upper bound on detection latency. Mount-unit
activation runs concurrently with sampling, not ahead of the timing window.
Readiness must have been present before replay and must still be absent after
activation completes; a late withdrawal remains a failure. After the first
withdrawal, sampling continues for ten seconds. Any observed reappearance is
latched in `endpoint_ready_reappeared` and fails the replay even if the watcher
removes the marker again before the final sample. The probe also records
`reappearance_latency_seconds` and `post_withdrawal_observation_seconds`.
This deterministic replay fails on images that remount over
populated `/Users`, even when the original restore timing race does not occur.

Replay diagnostics include `storage_watch_before` and `storage_watch_after`
(`systemctl show` fields `ActiveState`, `SubState`, `NRestarts`, and
`ExecMainStartTimestamp`, nested under `properties`), plus `storage_watch_journal` from the clone's current
boot in monotonic journal time. Each capture retains its return code, stdout,
and stderr, including diagnostic-command timeouts. A latency over three seconds
but under ten fails the withdrawal gate; because it includes `umount`, it does
not by itself establish that the watcher was slow. A `"never"` result with an inactive
or restarting watcher points to service failure; the journal can show its cause.
A running watcher with no unhealthy-storage message narrows the candidates to
a stalled loop or a storage view that did not change in its context, not proof
of either. An unhealthy-storage message without marker withdrawal instead
points to unlink failure or another process recreating the marker.

This keeps the server's reflink contract and cache behavior unchanged; a copy
fallback would broaden runner semantics and hide unsupported storage. The live
runner-host run (below) installed and used this volume for real jobs on the
original sandbox, on a clone created from its snapshot, and on that clone after
stop/resume. Sparse
capacity reserves no outer ext4 space: monitor **both** filesystems' free space.
The snapshot probe drains/stops the daemon, syncs, and unmounts `/Users` and XFS
before saving the disk snapshot; it never snapshots a live loop filesystem and
assumes consistency.

## Explicit Boat isolation profile

The operator's prerequisite pass measured systemd **255.4**, which rejects
`PrivatePIDs=yes`. `--boat-profile` is an explicit, **one-job-per-sandbox**
compatibility choice approved for this provider. The runner queries the actual
system manager's version. Without the flag, systemd <257 refuses startup; the
build server never silently loses PID isolation. Boat uses the runner init on
newer systemd too, to enforce its quota-inheritance filter. The flag requires the Linux host backend; tenant
mode additionally requires root-private local sockets rather than vsock.

In Boat mode, the unit runs the installed, root-owned
`/usr/local/bin/motor-runner job-pid-init <uid> <gid> <groups> <proc-reference-json> -- <wrapper argv>`.
The runner refuses startup if that installed executable is missing, unsafe or
different from the running executable. The init starts as root with only
`CAP_SYS_ADMIN CAP_SETUID CAP_SETGID CAP_SETPCAP` and `RestrictNamespaces=pid`,
creates a PID namespace, then forks its PID 1. That child mounts a fresh
`nosuid,nodev,noexec,hidepid=invisible` proc and moves systemd's protected proc
submounts into it. It drops to the exact `motor-job` UID, GID and supplementary
groups, empties all capability sets, joins a fresh session keyring as that user,
and stacks a filter restoring `RestrictNamespaces=yes` namespace restrictions
and protecting quota inheritance. Only then
does it exec the unchanged bash wrapper as PID 1; the command is PID 2.
`NoNewPrivileges=yes` and all other filesystem/network restrictions apply
throughout. Loader/allocator environment controls (`LD_*`, `GLIBC_TUNABLES`,
`GCONV_PATH`, `LOCPATH`, `MALLOC_*`) are refused before starting such a unit.

The root supervisor stays outside that PID namespace, in the same unit cgroup,
and mirrors the wrapper's exit or terminating signal. It uses one extra task
and some memory under the unchanged limits. The wrapper still reads the stdin
FIFO gate and writes the exit file. A SIGKILLed command reports 137, while a
SIGKILLed wrapper produces signal 9 for systemd's status fallback; OOM
classification still reads the unit cgroup's `Result`. Bootstrap failures
write their step and error to a root-only setup-error file. A complete report,
no wrapper exit code and no job output prove refused-before-start. If status
226 has neither a valid wrapper exit file nor complete setup evidence, the
outcome is unknown, never an assumed retry-safe refusal or job exit 226. A
wrapper-recorded exit 226 remains a normal exit. A supervisor wait failure uses
status 227 and cannot write a setup-failure record; without a wrapper exit file
it is an unknown outcome, never a retry-safe refusal. These changes apply only
when Boat mode is enabled, including Boat-plus-tenant deployments. Ordinary
build-server and non-Boat tenant units are unchanged.

The init starts a separate session/process group, re-arms its parent-death
SIGKILL after dropping credentials, and checks both PPid and an inherited
supervisor pidfd to close the reparenting race (PPid is 0 across PID namespaces).
The root supervisor closes the setup-record handle immediately after fork.
Only trusted pre-exec code writes that root-only file, either a failure line or
JSON telemetry recording credentials, post-drop PDEATHSIG, SID/PGID, descriptors
that survive exec and proc protections. The live root observer checks it and
the actual host-view session separation while a job runs.

Before serving Boat jobs, the runner measures protected proc mounts
in an independent systemd reference unit using the probe's protection policy.
The measured path/read-only set is passed in the init argv; after moving the
mounts, the init requires exact equality or refuses before start. No guessed
systemd mount list is embedded. clone3 explicitly returns ENOSYS in every
supported ABI, allowing libc to fall back to the checked clone call; an unknown
architecture is killed rather than relying on another filter. A child core
dump is never duplicated by the privileged supervisor: it mirrors the signal
with dumpability disabled, so systemd records killed-by-signal rather than the
child's core-dumped bit.

**What a Boat job sees in `/proc`.** The same as a build-server job: its own
process directories (other accounts' are hidden), plus the non-process entries
`/proc/stat`, `/proc/meminfo`, `/proc/cpuinfo`, `/proc/loadavg`,
`/proc/uptime`, `/proc/cmdline` and the rest. `ps` reads the boot time from
`/proc/stat`, and test harnesses and system-information crates read the others.
The unit does **not** set `ProcSubset=pid`: that hid all of these, so `ps`
failed with "Unable to get system boot time" and tools that work on the build
server failed only on Boat. Showing them costs no isolation. A Boat sandbox is
its own VM and admits exactly one job, so these entries describe only that
job's VM; on the build server jobs already see the shared host's entries.
`ProtectKernelTunables=yes` keeps `/proc/sys` and the other kernel-setting files
read-only, and the root-only entries (`/proc/kcore`, `/proc/kmsg`,
`/proc/sysrq-trigger`, ...) stay closed to the unprivileged job account.

The live job must record only its own UID/cgroup in `/proc`, only its own UID in
`ps`, a `/proc` mount with `hidepid=invisible` and without `subset=pid`, a
`btime` line in `/proc/stat` and a readable `/proc/meminfo`, a read-only
`/proc/sys`, no provider credential names in `/proc/cmdline`, zero
capabilities, and **ESRCH**, not EPERM, for `kill -0` on known outside PID
numbers with no local alias. The host records each control's UID, cgroup,
comm and stat starttime. A numeric alias passes only with the job's UID/cgroup
and a different starttime from the host identity, and is recorded as an alias.
In particular the sandbox's PID 1 becomes the job's own wrapper, not an
outside process accessible through that numeric name.
It must read `/proc/1/cgroup` equal to its own, see `bash` as PID 1 and itself
as PID 2, and match an ordinary systemd `User=motor-job` reference unit's
credentials, signal state and protected proc submounts. The probe records
keyring ID/UID/GID, all five capability sets, no-new-privileges and attempted
namespace syscalls. It also verifies normal exit 23, self-SIGKILL exit 137,
PID 1 ignoring an in-namespace SIGKILL, and cgroup teardown of a setsid child
under both stop and SIGKILL.
The host records runner, endpoint, sshd and provider-process PIDs as visibility
negative controls. **Exactly one job may be admitted per sandbox** remains
the provider policy, not a workaround for a missing PID boundary. Do not mix
tenants. Nested Firecracker using the existing
pinned tenant image is the follow-up for identical server isolation; the
prerequisite evidence shows KVM API 12 and AMD nested=1/SVM, not a boot or speed
qualification.

Everything else stays: job account, private network, private `/dev/shm`, strict
root/mount restrictions, capability dropping, memory ceiling, CPU placement and
KillMode=control-group teardown. Boat units additionally set CPUQuota per
requested vCPU and TasksMax=4096. No pressure workload is used to test limits:
the live job reads its own `memory.max`, `cpu.max` and `pids.max`.

Boat jobs have no shared compile cache (no sccache, no `/motor-cache`), but a
Rust job builds in the same fixed folder as on the build server:
`CARGO_TARGET_DIR=/run/motor/target`, bound to the workspace's own `target/`, so
`./target` and `/run/motor/target` are one directory. The live Rust job runs its
binary from `$CARGO_TARGET_DIR`, and the isolation job checks both facts.

The daemon's weight and rustc slots use `nproc`, measured by the start wrapper on every start. Its `--job-network veth` allows
network **only for explicit outbound requests**, through the existing private
network/veth machinery. A root pre-start reapplies only motor's nftables table
using the current IPv4 route's interface, enables forwarding and adds the narrow
UFW route allowance. It never opens a public runner port or flushes Boat's
ruleset. Missing prerequisites cause refusal, not host-network fallback.

**Boat-only memory rule:** round down to whole MiB:
`MemoryMax = MemAvailable immediately before runner startup - measured idle runner overhead - 512 MiB margin`.
The start wrapper runs a temporary, locally private bootstrap daemon, waits for its
owned listening socket, samples its service's `memory.current` ten times at idle,
then stops it and execs the final daemon with the measured cap. The entire daemon
service (including the small wrapper and charged startup cache) is counted,
conservatively. `/var/lib/motor-boat/budget.json` retains the raw baseline meminfo,
idle samples, margin and cap. The start wrapper refuses to launch if that computed job budget is below 1 GiB.
The reserved 512 MiB is headroom for the disposable loopback bridge and the
separately built authenticated endpoint, not a measured claim that their future peak usage will fit. The full probe records the final idle `motor-boat.service`,
`motor-boat-host.service` and `motor-boat-bridge.service` cgroups independently. ExecStartPost requires
the **final daemon PID** to own the listening socket before dependents start.

One job may use all measured usable vCPUs. The image is built on `large`, but
sandboxes of other sizes are created from it, so no CPU count is baked into the
image: on every start `motor-boat-start.py` passes the runner `--max-weight` and
`--rustc-slots` equal to the sandbox's `nproc`, and `--host-exec` writes
`/run/motor/sandbox.json`, a copy of `/etc/motor/sandbox.json` whose `vcpu_ceiling`
is that count, for the endpoint (the `/etc` value is the build sandbox's count).
The unit explicitly sets
`--reserve-daemon-core false`, so the sandbox's single job may use all its
vCPUs; this is not inferred from machine size and the fleet default still reserves
a daemon core where its existing topology policy applies. The fresh-resource operator pass
measured MemAvailable **15,492,481,024 B** on large (labels are not a capacity
promise). Creating xlarge returned HTTP 403 `machine_class_plan_required`,
requiring Boat's $100/month plan, so no xlarge capacity or behavior was measured. The memory
budget is remeasured on every service start/clone/resume, not baked into the
snapshot. The runner requires an explicit `--boat-memory-max-bytes` with the Boat
flag and refuses that override on the ordinary profile. **The build server's
16-GiB floor and 3.5-GiB-per-vCPU rule are unchanged.**

## SSH, identity and probe-only bridge

The SSH drop-in disables PTY/agent/X11/tunnel and remote TCP forwarding, retaining
local TCP forwarding. `sshd -t` must pass before reload. The probe records effective
`sshd -T` and drop-in contents initially and after stop/resume; persistence is a
measurement, not an assumption that Boat never manages these files. This is not
a forwarding-only login policy: command/shell access is not disabled by these
options. Production's independent token endpoint supplies admission control.

`/etc/motor/tenant/id` is absent in the base recipe and is removed/rechecked
immediately before snapshot capture. The broker writes the actual root-owned
0444 tenant identity on every create. The runner and admission endpoint run as root because the trusted tenant channels
must be root-owned in `/run/motor-tenant` (0700); jobs still run as `motor-job`.
The runner uses `--tenant-vm --tenant-socket-dir /run/motor-tenant`, never vsock in
the Boat profile. Readiness requires the daemon PID to own its ordinary socket
and all three root-0600 `jobs.sock`, `events.sock` and `admin.sock` listeners.
`motor-host --sandbox /etc/motor/sandbox.json` reads those channels and listens on
**127.0.0.1:9443**. Its root-owned config records the journal path, the measured
vCPU ceiling and the readiness marker path. `motor-boat-identity.service` starts
admission once the broker's identity file exists; endpoint readiness checks the
endpoint PID's loopback listener, and the readiness marker (below) tells the
broker when to book. The probe replays the broker's identity install, verifies an
actual `lease_refused/auth` frame for an invalid token, stops admission and
removes the identity before snapshot capture. This is a negative admission
check, not a successful broker-token job test: the endpoint trusts only the
production broker's JWKS, so the probe cannot mint a token it accepts. The
accepted-token path is covered in process by
`crates/motor-module/src/hosted/sandbox_tests.rs`.

The unauthenticated bridge is **probe-only**, loopback **127.0.0.1:19443** to the
ordinary runner Unix socket. Its unit is installed but never enabled; only the
probe starts it, stops it before snapshot, and restarts it explicitly on the
disposable clone. Never expose it as a production endpoint.

## Image contract: start-up after create and resume

What the broker relies on, in order. The units are installed only while the
image is built (`install.py`, followed by `daemon-reload` and
`systemd-analyze verify`); nothing rewrites a unit at runtime.

1. **Boat restores files after boot.** A sandbox created from a snapshot (and
   one resumed from archive) boots Boat's base image; Boat's agent then restores
   the snapshot's files. Until that finishes it serves `/etc`, `/usr`, `/srv`,
   `/var/lib/motor-boat` and other system directories from a FUSE filesystem
   (`ascii-lazyfs`), keeps `/var/lib/ascii-lazy/active`, and starts every
   enabled unit of the snapshot. When the restore is done Boat removes
   `/var/lib/ascii-lazy/active` and moves the real-disk directories into place (`hydrated` turns true about
   then). Observed on `bx_bph8zpug`: units started 3 min 27 s after boot, the
   switch to the real disk came 48 s later.
2. **`motor-boat-restored.service` gates everything.** The runner, its XFS
   mount (and so `/Users`), the endpoint and the identity watcher all require
    it. It waits until that file is gone, none of those directories is served
    by FUSE, and Boat's public storage binds are gone for five seconds, for at
    most 30 minutes. Starting earlier strands work on the FUSE
   view: the previous image's inotify path unit stayed armed on the retired
   `/etc` and never saw the broker's identity file, and the runner's first
    start failed because its state mounts were moved aside mid-start.
   **`motor-boat-storage.service` then establishes the actual disk mounts.**
   Boat's final restored view can itself be an ext4 bind mount at
   `/var/lib/motor-boat`. systemd counts that as an active mount unit even
   though it is not the unit's configured XFS disk. The previous image already
   required both mount units, but those dependencies were satisfied by Boat's
   binds: the runner wrote state on ext4, and a later job's mount activation
   hid its workspace and stdin FIFO. The storage service is a direct requirement
    of both runner and endpoint. It starts the mount units itself, after checking
    both exposed directories and confirming neither consumer has a live PID.
    After storage evidence is published, a mount-unit condition blocks later
    activation; it cannot
    silently repair storage underneath a live runner.
   `/run/motor-boat-storage.json` records verified mount identities, boot ID and
   monotonic time before either service starts. The image snapshot procedure
   stops services and mount units and checks both exposed directories are empty.
3. **Identity.** The broker writes `/tmp/ck-motor-tenant-id` through Boat's file
   API, then runs `sudo install -D -m 0444 -o root -g root /tmp/ck-motor-tenant-id
   /etc/motor/tenant/id && rm -f /tmp/ck-motor-tenant-id && cat /etc/motor/tenant/id`
   after every create and resume. `motor-boat-identity.service` polls for that
   path once a second with no deadline: by path, not inotify, so it works
   whether the file is written in place or renamed in, whether it predates the
   watcher, and whatever Boat did to the mounts before. An install that landed
   on the FUSE view and was lost in the switch simply waits for the broker's
   next install. Starting an already-running endpoint is a no-op, so repeated
   installs never start a second endpoint.
4. **Readiness marker.** `motor-host` writes `/run/motor/endpoint-ready`
   (root, 0644, tmpfs, so per boot) only while it listens on 127.0.0.1:9443,
   holds a loaded broker JWKS and the identity file is valid, and the file
   holds exactly that identity (no newline). It is rechecked every second: a
   missing or invalid identity removes it, a changed identity rewrites it, and
   a failed JWKS fetch is retried every 5 s. Stopping the endpoint removes
   `/run/motor` with it. The broker reads it with `cat /run/motor/endpoint-ready`
   through Boat's exec API as the sandbox user and books only on exit 0 with
   stdout equal to the sandbox id (optionally plus a newline).
   `/run/motor/endpoint-revision` holds the image's source revision.

The restart that used to rescue the runner's failed first start is no longer
needed for it: start deadlines are 120 s (bootstrap and readiness) inside a 300 s
`TimeoutStartSec`, and `Restart=on-failure` is only a backstop.

`systemctl status` on a restored sandbox may still warn that our units (and
Boat's own `ariana-fswatch-*` units) "changed on disk": systemd loaded them
through the FUSE view, whose file times differ from the real disk's. The content
is the same file; units restored by other means (`ssh.service`) do not warn. It
is a Boat restore artifact, not a rewrite, and the image does not reload systemd
at runtime to hide it.

## Disk budget

Recursive mode repair is implemented by the runner's native `physical-chmod`
operation, not by a version-dependent coreutils `chmod -P`. It pins every
ancestor with `openat(O_PATH|O_DIRECTORY|O_NOFOLLOW)` and each root with
`O_PATH|O_NOFOLLOW`. A root may be a directory (walked recursively) or a regular
file (only that inode); a symlink or special-inode root is refused before any
mode changes. It opens children with
`O_NOFOLLOW`, skips symlink inodes by `fstat`, and changes modes through pinned
descriptors. The kernel-owned `/proc/self/fd` link is used for `fchmodat` so FIFO,
socket and device inodes need not be opened for I/O; it never resolves a
job-controlled symlink. Trusted sudo invokes the fixed root0755 copy at
`/usr/local/libexec/motor-runner-permissions`, never a caller-selected executable.
The fleet's narrow operator-only sudo template and administrator installer are
under `script/runner/`; deploy refuses a new candidate until helper ownership,
code digest and both exact command forms are valid. Boat runs the trusted unit
as root and installs that same fixed helper, without granting jobs sudo. No
setuid helper or shell command is delegated.

The Boat unit explicitly sets a **24 GiB admission floor** and **32 GiB soft
eviction goal**; neither is inferred from the provider label or filesystem size.
An operator-reported cold Cargo build used about 20 GiB, so 24 GiB reserves that
footprint plus 4 GiB of working headroom rather than admitting a cold build with
only 4 GiB free. The 32 GiB goal starts reclaiming inactive caches earlier. Both
fit the roughly 63 GiB free measured inside this 64 GiB XFS volume. These are
admission/reclamation thresholds, not a disk quota or a guarantee for arbitrary
artifact sizes; active workspaces and cache-ownership protections are unchanged.
The fleet defaults remain 128 GiB hard / twice that (256 GiB) soft. The existing
`--free-space-floor-bytes` flag sets the floor; new
`--free-space-eviction-bytes` optionally overrides the soft goal and must exceed
the floor, otherwise the original twice-floor rule applies.

### Per-job disk quota

The Boat state mount uses XFS `prjquota,discard`; startup refuses a mount without
project enforcement. `discard` returns freed loop-image extents to the outer
filesystem, rather than leaving an ever-growing sparse file after eviction.
Existing images must be rebuilt/remounted while services are stopped; a live
remount cannot safely enable XFS project quotas.

For **Boat only**, after trusted checkout/dependency preparation and before job
code, the runner assigns the workspace (including its target bind), private
runtime (HOME, temporary directories, raw stdout/stderr and the preallocated
exit inode), and incoming spool to one project. Shared Cargo registry/Git,
rustup and tool binaries are read-only job mounts. Private Cargo/Bun cache writes
are under HOME. `/dev/shm` is a separate, memory-cgroup-bounded tmpfs: it cannot
be charged to XFS and remains memory-limited instead. Boat explicitly binds
`/var/tmp` to the same XFS scratch as `/tmp`, avoiding any dependency on the
systemd version's private-temporary backing. The live probe verifies project IDs
on both spellings. Binds of an unexpected
filesystem fail setup, rather than silently escaping the quota.

Additional growth is capped at 32 GiB and calculated from
`min(XFS available bytes, / available bytes) - provisional reserve - replay tail
allowance - outstanding job reservations`, rounded down to 512 bytes. The **24
GiB snapshot/cleanup reserve is provisional**, reusing the existing admission
floor; it is **not** a measured upper bound on Boat's snapshot staging. The
runner also withholds the existing 64-MiB replay-tail maximum from growth.
Already-present workspace blocks are charged as initial usage, not mistakenly
treated as free growth. Inodes have a hard additional cap of 65,536, leaving
8,192 available for runner bookkeeping. A flock serializes allocation and
cleanup, and durable leases retain outstanding reservations across restart.

Block/inode hard-limit failures stay inside the job as EDQUOT (or ENOSPC).
Start logs include both filesystems' available bytes and the job allowance.
The kernel's `VFS_DQUOT` project hard-limit notifications produce a warning and
a private hit marker even if the job subsequently unlinks its fill file;
cleanup logs whether a warning was observed. This requires the provider kernel's
`CONFIG_QUOTA_NETLINK_INTERFACE`; quota setup fails closed if its events group
is unavailable. Multicast notifications are observational, not a lossless audit
log across runner crashes. The exit file's data block is allocated before job
code and charged as initial project usage, so recording an exit overwrites
existing storage. The separate runner reserve file is outside the project and
remains available for terminal bookkeeping. After the cgroup is gone, runtime
cleanup clears tree assignments, limits, hit markers and the lease. Workspace
contents remain reusable and subject to normal idle/pressure eviction.

Runner-written replay chunks/records and returned-file copies are not job
writable mounts and retain their existing bounds; the fixed reserve protects
them. Trusted dependency fetching and shared package-manager cache preparation
precede quota installation and are **not** bounded by this job-code quota. A
future writable bind from another filesystem must move onto this XFS volume or
have an independently enforced filesystem quota deducted from the same budget;
there is no unbounded writable-bind fallback.

### Project-inheritance protection

Boat jobs always enter through the runner's PID namespace init, even when the
system manager supports `PrivatePIDs`. Its existing seccomp filter also rejects
filesystem-attribute setter ioctls (`FS_IOC_FSSETXATTR`, native and compat
`FS_IOC_SETFLAGS`) with `EPERM`. An inode owner can otherwise clear XFS project
inheritance on a newly created directory and allocate uncharged project-0 files.
The x86-64, x32 and i386 paths are covered; attribute getters and unrelated
ioctls remain allowed. `chattr` **setting** flags may notice this restriction;
normal builds do not need these setters. Non-Boat build-server and tenant units are
unchanged. The filter interpreter exercises both sides of every jump, and a
forked-child test checks actual setter/getter syscalls. The operator disk-quota
probe also attempts to clear inheritance before filling the same directory.

## Fast startup checks and failure evidence

Before long tool builds, the installer dry-runs the network helper: it requires
absolute `/usr/sbin/ip`, `nft`, `sysctl` and `ufw` paths, validates the nft transaction
and checks UFW route syntax without applying rules or sysctl writes. The root
service PATH also includes administrative directories. After installation,
`systemd-analyze verify` checks the units and an actual start/stop of the runner
unit exercises its exact flags, the restore gate, mounted state, root account
validation, memory budget and all required socket listeners without exposing a
job connection. The endpoint still waits for the probe/broker identity file.

Any failed guest command records `systemctl status --no-pager -l` and 200 boot
journal lines separately for the restore gate, runner, endpoint, identity watcher and bridge units under
`runner_host.guest_failures[].unit_diagnostics`; collection errors and raw command
results are retained rather than silently discarded. The recipe's startup
preflight prints the same per-unit evidence before stopping a failed service.

## Operator probe

From the operator machine, use the same stdin credential reader as `boat-probe`:

```sh
set -o pipefail
<key reader> | python3.14 script/boat-probe/probe.py --key-stdin --runner-host \
  --output .cortexkit/internal-docs/research/boat-runner-host --max-minutes 30
```

To test an **existing prepared snapshot unchanged**, including the production
create/hydrate/identity order, use the qualification-only mode:

```sh
set -o pipefail
<key reader> | python3.14 script/boat-probe/probe.py --key-stdin --runner-host \
  --verify-prepared-image motor-boat-prepared-213a82180779 \
  --output .cortexkit/internal-docs/research/boat-storage-old-image --max-minutes 30
```

The old image above must fail storage checks. Substitute the newly prepared
snapshot handle and a fresh output directory to qualify the fix. Unlike
`--from-prepared-image`, this mode **does not reapply the recipe, restart the
runner/watcher, or repair mounts** before checking it. It creates a clone,
waits for hydration, installs the identity with the broker's exact commands,
checks readiness and disk identity/order, runs real jobs, and repeats after
stop/resume. Only the probe-only bridge is explicitly started to transport real
jobs. The source snapshot is borrowed and never deleted by cleanup.

Every real-job fixture includes an npm lockfile and a locally generated tarball
dependency containing regular files. No registry or project-specific dependency
is needed. The first and second Node jobs read `node_modules/boat-dep/index.js`;
publishing and reusing the install cache must exercise the runner's reflink
path, rather than cloning an empty directory or symlinks only. Mount evidence
includes current findmnt output and loop backing identity, plus the per-boot
   storage verification timestamp preceding both runner and endpoint start times.

Qualification also rejects stacked entries at either target; it does not assume
the first `findmnt` row is the effective mount. The runner's Linux clone path
checks the source and nearest existing destination parent's actual `st_dev`
before creating a target, so hidden mount-table entries cannot substitute for
the kernel's path resolution. FICLONE itself is still authoritative and there
is no copy fallback.

Expect about **10 minutes** of sandbox time when starting from a prepared image
(the measured run below took 9 minutes plus asynchronous purge), and longer for a
cold first build of every tool; the cold path was not timed in that run. The
operator machine first builds the small `boat_probe` example;
no paid sandbox exists during that local build. It uses `Motor::with_connector`
and `Motor::run`, so snapshot capture, bundle transfer, admission, streamed output
and terminal handling are production client code, not a Python wire imitation.
The only custom transport is TCP to an API-host-key-pinned SSH local forward.

One original is created with noEnv, source is uploaded over pinned SSH and
checksum-verified, and the recipe builds that source commit. Real jobs test exit
23, an offline Rust build, identity, lo-only networking, private shm, the private
PID layout and the three cgroup files. The probe audits account credentials
before snapshot, saves a named snapshot, deletes the original, and only after
completed deletion or the provider's stopped `waiting_for_uploads` stage creates
the clone, also with noEnv. It measures create request → first real terminal
received, including hydration, SSH and service readiness; then tests stop/resume
and effective SSH policy again. Never two live sandboxes. Measurement time is
capped at 30 minutes; deletion/purge latency is separate.

The endpoint start-up path is replayed in the broker's order, with the
broker's exact identity install (see the image contract) and its exact marker
read, `cat /run/motor/endpoint-ready` as the sandbox user, which must print the
sandbox id. Each wait has a hard limit of 120 s and its measured seconds are
recorded; the worst observed so far is 3.3 s (see the measured run below):

- **original and clone, watcher armed first:** stop the endpoint, remove the
  identity, restart the watcher, confirm no marker and nothing on 9443, then
  install. The marker must appear root 0644 with the id.
- **clone, broker order:** install right after the create returns, while Boat
  may still be restoring files; wait for `hydrated`; then wait for the marker.
  If that early install was lost in Boat's switch to the real disk, install
  again (as the broker does) and wait again. Results record whether the early
  install survived.
- **resumed clone, identity already present:** the file is on disk before the
  watcher starts, so the marker must appear without a new install; a repeated
  install must keep the same endpoint PID and restart count.

The audit explicitly checks every passwd account home for Boat's in-sandbox API
key, classifies `.npmrc` by **key names only**, and records sensitive environment
names without values. Before tools are built and on every runner start, a root
hygiene step records only the CLI config's JSON key names and nonempty
credential-like field names, deletes that config for every account, and replaces
provider exports in shell profiles with no-op statements (preserving shell
conditional syntax). No provider CLI config values or provider environment values
are emitted. The initial
and per-start redacted reports retain PID/UID/comm/cgroup and provider-variable
names for PID 1 and processes carrying ASCII_TOKEN, plus actual `motor-job`
file/proc readability checks outside the job sandbox. The real isolation job
repeats those access checks inside its sandbox. Empty/missing evidence is not a
passing access test.

Runner and endpoint exec wrappers clear `ASCII_TOKEN` and every `BOAT_*` key;
transient job units also unset the provider names discovered from the system
manager/caller environment before admission. Unknown future `BOAT_*` names are
covered. Credential absence is checked again after clone and resume.
[Boat's security documentation](https://docs.boat.dev/security) scopes ASCII_TOKEN only to hosting ports of its own
sandbox, not to the account or other sandboxes; the process/readability probes
measure exposure, not independent authorization scope. Config existence alone
is not proof of a populated API key: the redacted field evidence must establish
that distinction. The noEnv in-sandbox CLI key is removed regardless.

The audit retains SDK source-file lookalikes separately rather
than claiming their filenames contain credentials. Documented provider-only
`ASCII_TOKEN`/`BOAT_ID` are recorded separately; neither may enter a motor job.
Audit errors/unknown npmrc syntax prevent snapshotting. It is a bounded scan, not
proof against deliberately hidden arbitrary secrets. SSH and systemd token
inheritance are measured independently. Cleanup/recovery and accepted-versus-final
purge reporting are the existing `script/boat-probe/README.md` contract.

Live-mode stdout is one JSON report even on fatal setup/argument failures;
incidental local command progress goes to stderr. A refused isolation job is a
failed check with its terminal/refusal reason, not an attempt to parse its empty
guest stdout, and a refused job is never counted as snapshot-to-first-job timing.

## Optional prepared image retention

`--prepared-image` explicitly opts into saving a tool-populated base as
`motor-boat-prepared-<digest-short>`. The digest includes the exact source commit,
installed package versions and signed Release metadata, so live-archive rebuild
inputs are identified rather than silently overwriting a previous base. A ready
image with that same name is reused, not replaced. Results record its name,
`id` (Boat snapshotId), restored-content size, full digest and exact deletion
invocation; pipe the API-key reader command from the **Operator probe** example
above into that deletion invocation's stdin.
Named snapshot storage continues to be billed until the operator deletes it.

Before any new prepared or measurement snapshot, the probe requires no tenant
identity, no Boat CLI configs/credential candidates and no authorization for its
throwaway SSH public key; no private SSH key is ever uploaded. It stops the
services, syncs and unmounts the XFS state. Prepared capture happens before any
real job, so its result explicitly remains unqualified if subsequent job checks
fail. This is a retry cache, not a claim that jobs already passed.

On a later run, `--from-prepared-image NAME` creates a fresh noEnv large from that
operator-owned base, reapplies the current source/recipe using the persisted
Cargo/tool caches, and still performs **all** real-job, security, snapshot and
resume checks. It never treats cache restoration as a substitute for them.
Every sandbox and measurement snapshot is deleted normally. Only the explicitly
retained prepared image is excluded from requested cleanup; it is listed under
`operator_retained_resources` and the summary states that it was not deleted.
`--cleanup RESULTS.json --delete-retained-images` explicitly deletes it too.
No image is retained by default, and a borrowed source image is not owned/deleted
by the new run.

## Endpoint start-up measured on Boat (2026-10-20)

One `--runner-host --prepared-image --from-prepared-image
motor-boat-prepared-cbc6493de2f6` run at source revision `4420d92` passed 133 of
133 checks and retained the prepared image **`motor-boat-prepared-5dab50642cb0`**
(qualified by its real jobs). Seconds from the broker's identity install (or
from `hydrated`, when the identity was already there) until
`cat /run/motor/endpoint-ready` printed the sandbox id:

| Phase | Order | Seconds |
| --- | --- | --- |
| initial | watcher armed, then install | 3.3 |
| snapshot clone | broker order, install sent before the restore finished | 2.9 |
| snapshot clone | watcher armed, then install | 0.9 |
| resumed | identity already present | 0.5 |
| resumed | re-install while serving | 0.9 |

On the clone and after resume, `motor-boat.service` and `motor-boat-host.service`
reported `NRestarts=0`: the first start succeeded, without the restart that
rescued the previous image. The early install, sent right after create,
survived Boat's switch to the real disk in this run, so the re-install branch
was not exercised. Create from snapshot to hydrated took 43.1 s and to the
readiness marker 46.0 s. The 120 s probe limit stays the hard bound; the broker
itself waits up to 10 minutes.

## Measured on Boat (2026-10-19)

One `--runner-host --prepared-image --from-prepared-image
motor-boat-prepared-99da3e87bb31` run at source revision `5ffcb1a`, on `large`
(8 vCPU, 16 GB), 21:57–22:06 UTC. Raw evidence is the run's `results.json`; its
`results.md` is rendered from it. The verdict **passed**: 107 of 107 checks, 34 in
each of the `initial`, `snapshot_clone` and `resumed` phases plus four snapshot
hygiene checks and `sshd.dropin_survives_resume`.

This historical run predates the per-job PID bootstrap. Its shared-PID and
EPERM observations below do not qualify the current private-PID profile;
the operator must rebuild and rerun the expanded probe for that evidence.

- **Real jobs, through the production client** over the API-pinned SSH forward:
  the exit job returned code 23 with its output, the offline Rust 1.99.0 build
  printed `boat-hello`, in every phase. Each job took 1.0–1.4 s end to end from
  the operator machine, 2.5–4.1 s right after resume.
- **Isolation, read by the job itself:** user `motor-job`, interfaces `["lo"]`, a
  network and mount namespace different from the host's, private writable
  `/dev/shm`, NoNewPrivs with zero effective and bounding capabilities, `/proc`
  mounted `subset=pid` with hidden other-user processes (this run predates
  dropping `ProcSubset=pid`; see the isolation profile), `ps` showing only its
  own processes, EPERM signalling the runner, and no Boat token in its
  environment or readable files.
- **PID namespace: not private.** The job's `pid` namespace equals the host's
  (`pid:[4026531836]` in all phases). Boat's systemd 255 has no
  `PrivatePIDs=yes`; that older profile substituted the `/proc` and signal
  controls above. The current profile uses `job-pid-init` instead.
- **cgroup limits** from the job's own `memory.max`, `cpu.max`, `pids.max`:
  `cpu.max` `800000 100000` (8 vCPUs) with `nproc` 8, `pids.max` 4096, and
  `memory.max` equal to the start wrapper's budget:

  | Phase | MemAvailable before runner | Runner idle | memory.max | MiB |
  | --- | --- | --- | --- | --- |
  | initial (prepared image + recipe) | 14,657,724,416 | 14,467,072 | 14,105,444,352 | 13452 |
  | snapshot_clone (first start of a clone) | 14,627,483,648 | 48,721,920 | 14,041,481,216 | 13391 |
  | resumed (clone after stop/resume) | 14,649,823,232 | 15,581,184 | 14,097,055,744 | 13444 |

  The budget is remeasured on every start, so it varies by tens of MiB. The
  first start of a clone measured the runner at 48.7 MB idle instead of about
  15 MB, giving the lowest budget, 13391 MiB. Anything that advertises a fixed
  job memory for Boat must use the lowest observed value, not 13444.
- **Timing** (from the operator machine): create from the prepared image to Boat-ready 2.7 s;
  source upload plus recipe reapply 63.6 s; prepared image save 12.0 s;
  measurement snapshot save 19.1 s; create from snapshot to Boat-ready 5.4 s and
  **to first finished job 68.1 s**. About 50 s of the 68.1 s is waiting for
  Boat's `hydrated` flag (ready at 5.2 s, hydrated at 55.8 s), which the probe
  requires before using the sandbox; whether jobs can safely start before full
  hydration was not measured. Stop to `archived` took 75.7 s; resume to ready
  8.3 s and to hydrated 93.7 s.
- **Images:** the prepared image `motor-boat-prepared-5c81106dd6cc` (snapshot
  `d37f0a2c-5bf5-4b3d-ab59-aaf661d66480`, 8,718,008,320 bytes) is retained and
  qualified by these real jobs. The measurement snapshot (8,727,703,552 bytes)
  was deleted. The earlier base `motor-boat-prepared-99da3e87bb31`
  (8,764,968,960 bytes) was only borrowed as this run's source and is no longer
  needed.
- **Cleanup:** both sandboxes' deletions were accepted and wait for upload URL
  expiry (`waiting_for_uploads`, purge expected about six hours later). Provider
  usage: 352 s / $0.00352 and 377 s / $0.00377. That pending purge made the old
  harness exit 1 despite a passing verdict; the harness now exits on the verdict
  and reports `cleanup_state` separately.

Portable tests run via `python3 scripts/run-script-tests.py`. They validate pins,
checksum rejection and evidence parsing, **not** live installation or isolation.
