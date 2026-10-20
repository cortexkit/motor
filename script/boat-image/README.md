# Boat runner host recipe

Apply as root in a fresh **Ubuntu 24.04 x86_64 Boat `large`**, created with
`noEnv: true`. Do not attach account secrets, credentials, repositories or CLI
logins. The recipe needs only public downloads and never upgrades/replaces the
provider's init or sshd binaries. The source checkout must be clean and at the
exact revision being installed:

```sh
sudo python3 script/boat-image/install.py --revision "$(git rev-parse HEAD)"
sudo systemctl start motor-boat motor-boat-identity
```

`pins.json` fixes the official signed Ubuntu source URLs, Rust/rustup, Node,
Bun, pnpm and NATS versions, and hashes the existing Cargo-tool and sccache
build inputs. Release archives are checked against their publishers' SHA-256
lists; pnpm uses the exact version's published SHA-512 integrity. Rustup and
Cargo verify distribution/crate checksums; git tools use the full revisions in
`script/runner/tools.txt` and locked dependencies. The patched sccache is built
with `script/runner/build-sccache.sh`; its read-only-cache patch is required
because stock sccache touches cache files even for a hit. Anonymous commons
access is tested inside Boat before installing its nine pinned mutation tools.
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

## Filesystem choice

Boat's measured ext4 root rejects FICLONE (`EOPNOTSUPP`). Use a **64 GiB sparse,
loop-mounted XFS image** on that disk, with `reflink=1`, not a plain-copy runner
fallback. The state, warm workspaces, JS install caches and `/Users` workspace
bind all share that filesystem. The recipe actually clones random nonempty data
between the state mount and workspace bind, and checks the bytes. A loop-image
or ioctl failure aborts installation rather than changing copy semantics.

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
build server never silently loses PID isolation. On newer systemd even the Boat
profile retains PrivatePIDs. The flag requires the Linux host backend; tenant
mode additionally requires root-private local sockets rather than vsock.

On systemd 255, jobs share the sandbox's PID namespace. The per-job PID
visibility boundary is lost: jobs using the same `motor-job` UID can see and
signal each other's processes. The daemon also inhabits that namespace, but
**ProtectProc=invisible and its different UID remain in force**; shared namespace
does not grant permission to signal the daemon or override that `/proc` filter.
The Boat job unit sets `ProtectProc=invisible`, `ProcSubset=pid`,
`User=motor-job`, `Group=motor-job`, `NoNewPrivileges=yes`, an empty
`CapabilityBoundingSet`, `RestrictNamespaces=yes` and `MountAPIVFS=yes`.
`ProtectProc` hides other users' process directories; `ProcSubset` hides non-PID
procfs entries; the UID/capability rules deny signals to root and provider-user
processes. These are not a replacement PID namespace against another process
with the same UID. The live job must record only its own UID/cgroup in `/proc`,
only its own UID in `ps`, the actual proc mount options and zero capabilities,
and get **EPERM**, not ESRCH, for `kill -0` on the known live runner PID.
The host records runner, endpoint, sshd and provider-process PIDs as visibility
negative controls. **Exactly one job may be admitted per sandbox**: these
controls do not restore a same-UID PID-namespace boundary. Do not mix tenants. Nested Firecracker using the existing
pinned tenant image is the follow-up for identical server isolation; the
prerequisite evidence shows KVM API 12 and AMD nested=1/SVM, not a boot or speed
qualification.

Everything else stays: job account, private network, private `/dev/shm`, strict
root/mount restrictions, capability dropping, memory ceiling, CPU placement and
KillMode=control-group teardown. Boat units additionally set CPUQuota per
requested vCPU and TasksMax=4096. No pressure workload is used to test limits:
the live job reads its own `memory.max`, `cpu.max` and `pids.max`.

The daemon's weight and rustc slots use `nproc`. Its `--job-network veth` allows
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

One job may use all measured usable vCPUs; this recipe targets `large` only and
uses its measured `nproc=8` for weight/slots. The unit explicitly sets
`--reserve-daemon-core false`, so the sandbox's single job may use all eight
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
   it. It waits until that file is gone and none of those directories is served
   by FUSE, for at most 30 minutes. Starting earlier strands work on the FUSE
   view: the previous image's inotify path unit stayed armed on the retired
   `/etc` and never saw the broker's identity file, and the runner's first
   start failed because its state mounts were moved aside mid-start.
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

From the operator's Mac, use the same stdin credential reader as `boat-probe`:

```sh
set -o pipefail
<key reader> | python3.14 script/boat-probe/probe.py --key-stdin --runner-host \
  --output .cortexkit/internal-docs/research/boat-runner-host --max-minutes 30
```

Expect about **10 minutes** of sandbox time when starting from a prepared image
(the measured run below took 9 minutes plus asynchronous purge), and longer for a
cold first build of every tool; the cold path was not timed in that run. The Mac
first builds the small `boat_probe` example;
no paid sandbox exists during that local build. It uses `Motor::with_connector`
and `Motor::run`, so snapshot capture, bundle transfer, admission, streamed output
and terminal handling are production client code, not a Python wire imitation.
The only custom transport is TCP to an API-host-key-pinned SSH local forward.

One original is created with noEnv, source is uploaded over pinned SSH and
checksum-verified, and the recipe builds that source commit. Real jobs test exit
23, an offline Rust build, identity, lo-only networking, private shm, the explicit
shared PID profile and the three cgroup files. The probe audits account credentials
before snapshot, saves a named snapshot, deletes the original, and only after
completed deletion or the provider's stopped `waiting_for_uploads` stage creates
the clone, also with noEnv. It measures create request → first real terminal
received, including hydration, SSH and service readiness; then tests stop/resume
and effective SSH policy again. Never two live sandboxes. Measurement time is
capped at 30 minutes; deletion/purge latency is separate.

The endpoint start-up path is replayed in the broker's order, with the
broker's exact identity install (see the image contract) and its exact marker
read, `cat /run/motor/endpoint-ready` as the sandbox user, which must print the
sandbox id. Each wait is bounded by 120 s and its measured seconds are recorded:

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

## Measured on Boat (2026-10-19)

One `--runner-host --prepared-image --from-prepared-image
motor-boat-prepared-99da3e87bb31` run at source revision `5ffcb1a`, on `large`
(8 vCPU, 16 GB), 21:57–22:06 UTC. Raw evidence is the run's `results.json`; its
`results.md` is rendered from it. The verdict **passed**: 107 of 107 checks, 34 in
each of the `initial`, `snapshot_clone` and `resumed` phases plus four snapshot
hygiene checks and `sshd.dropin_survives_resume`.

- **Real jobs, through the production client** over the API-pinned SSH forward:
  the exit job returned code 23 with its output, the offline Rust 1.99.0 build
  printed `boat-hello`, in every phase. Each job took 1.0–1.4 s end to end from
  the Mac, 2.5–4.1 s right after resume.
- **Isolation, read by the job itself:** user `motor-job`, interfaces `["lo"]`, a
  network and mount namespace different from the host's, private writable
  `/dev/shm`, NoNewPrivs with zero effective and bounding capabilities, `/proc`
  mounted `subset=pid` with hidden other-user processes, `ps` showing only its
  own processes, EPERM signalling the runner, and no Boat token in its
  environment or readable files.
- **PID namespace: not private.** The job's `pid` namespace equals the host's
  (`pid:[4026531836]` in all phases). Boat's systemd 255 has no
  `PrivatePIDs=yes`, so this profile substitutes the `/proc` and signal controls
  above and admits exactly one job per sandbox (see the isolation profile).
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
- **Timing** (from the Mac): create from the prepared image to Boat-ready 2.7 s;
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
