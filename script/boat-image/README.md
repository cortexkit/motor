# Boat runner host recipe

Apply as root in a fresh **Ubuntu 24.04 x86_64 Boat `large`**, created with
`noEnv: true`. Do not attach account secrets, credentials, repositories or CLI
logins. The recipe needs only public downloads and never upgrades/replaces the
provider's init or sshd binaries. The source checkout must be clean and at the
exact revision being installed:

```sh
sudo python3 script/boat-image/install.py --revision "$(git rev-parse HEAD)"
sudo systemctl start motor-boat
```

`pins.json` fixes the dated, signed Ubuntu package archive, Rust/rustup, Node,
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
These are reproducible input pins, not a claim that independently linked Rust
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
profile retains PrivatePIDs. The flag is refused for non-Linux, VM and tenant-VM
CLI configurations.

On systemd 255, jobs share the sandbox's PID namespace. The per-job PID
visibility boundary is lost: jobs using the same `motor-job` UID can see and
signal each other's processes. The daemon also inhabits that namespace, but
**ProtectProc=invisible and its different UID remain in force**; shared namespace
does not grant permission to signal the daemon or override that `/proc` filter.
Do not use this profile to mix tenants. Nested Firecracker using the existing
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
separately built authenticated endpoint, not a measured claim that their future
peak usage will fit. The full probe records the final idle `motor-boat.service`
and `motor-boat-bridge.service` cgroups independently. ExecStartPost requires
the **final daemon PID** to own the listening socket before dependents start.

One job may use all measured usable vCPUs; this recipe targets `large` only and
uses its measured `nproc=8` for weight/slots. The fresh-resource operator pass
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
0444 tenant identity on every create. Port **9443 stays free** for the separately
built endpoint; that endpoint is deliberately not started here.

The unauthenticated bridge is **probe-only**, loopback **127.0.0.1:19443** to the
ordinary runner Unix socket. Its unit is installed but never enabled; only the
probe starts it, stops it before snapshot, and restarts it explicitly on the
disposable clone. Never expose it as a production endpoint.

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
names without values. It retains SDK source-file lookalikes separately rather
than claiming their filenames contain credentials. Documented provider-only
`ASCII_TOKEN`/`BOAT_ID` are recorded separately; neither may enter a motor job.
Audit errors/unknown npmrc syntax prevent snapshotting. It is a bounded scan, not
proof against deliberately hidden arbitrary secrets. SSH and systemd token
inheritance are measured independently. Cleanup/recovery and accepted-versus-final
purge reporting are the existing `script/boat-probe/README.md` contract.

Portable tests run via `python3 scripts/run-script-tests.py`. They validate pins,
checksum rejection and evidence parsing, **not** live installation or isolation.
