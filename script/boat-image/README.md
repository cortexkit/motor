# Boat runner host recipe

Apply as root in a fresh **Ubuntu 24.04 x86_64 Boat `large`**, created with
`noEnv: true`. Do not attach account secrets, credentials, repositories or CLI
logins. The recipe needs only public downloads and never upgrades/replaces the
provider's init or sshd binaries. The source checkout must be clean and at the
exact revision being installed:

```sh
sudo python3 script/boat-image/install.py --revision "$(git rev-parse HEAD)"
sudo systemctl start motor-boat motor-boat-host.path
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
fallback would broaden runner semantics and hide unsupported storage. Loop/XFS
availability and actual warm/cache use still need the live job probe. Sparse
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
**127.0.0.1:9443**. Its root-owned config records the journal path and measured
vCPU ceiling. A systemd path unit starts admission only after the broker writes
the identity file; endpoint readiness checks the endpoint PID's loopback listener.
The probe installs a temporary root-0444 test identity, verifies an actual
`lease_refused/auth` frame for an invalid token, stops admission and removes the
identity before snapshot capture. This is a negative admission check, not a
successful broker-token job test.

The unauthenticated bridge is **probe-only**, loopback **127.0.0.1:19443** to the
ordinary runner Unix socket. Its unit is installed but never enabled; only the
probe starts it, stops it before snapshot, and restarts it explicitly on the
disposable clone. Never expose it as a production endpoint.

## Disk budget

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
unit exercises its exact flags, mounted state, root account validation, memory
budget and all required socket listeners without exposing a job connection.
The endpoint still waits for the probe/broker identity file.

Any failed guest command records `systemctl status --no-pager -l` and 200 boot
journal lines separately for every runner, host, host-path and bridge unit under
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

Expect **20–30 minutes** for cold tool builds/downloads, plus cleanup (unmeasured
until the operator runs it). The Mac first builds the small `boat_probe` example;
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

Portable tests run via `python3 scripts/run-script-tests.py`. They validate pins,
checksum rejection and evidence parsing, **not** live installation or isolation.
