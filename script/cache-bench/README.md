# Operator-only compile-cache placement benchmark

These scripts measure a pinned motor workspace against several S3-compatible
backends. They do not configure production caches. Run A **directly over SSH** on
the Linux build server, not as a runner job. Run B in a disposable Boat clone of
the current prepared image through `probe.py --cache-bench`. No live speed result
is included; runs A and B require operator access and provider credentials.

## Backends and credentials

Copy `backends.example.json` to an operator-owned location. It describes four
independent backend/class fills; bucket names are per backend:

| Backend name | Bucket | Endpoint | Region | Addressing | Requested write class |
|---|---|---|---|---|---|
| ovh-standard | motor-cache-test | https://s3.de.io.cloud.ovh.net | de | path | STANDARD |
| ovh-high-performance | motor-cache-test | https://s3.de.io.cloud.ovh.net | de | path | EXPRESS_ONEZONE |
| r2 | motor-cache-test | https://f5b0dccc2a21bdcbcaeb1dcd690a8d38.r2.cloudflarestorage.com | auto | path | unspecified/default |
| hetzner-fsn1 | cortexkitmotor | https://fsn1.your-objectstorage.com | fsn1 | path | unspecified/default |

Each `credentials` path names an owner-only (`chmod 600`), regular, three-line
file: **HTTPS endpoint, access ID, secret**. R2 also accepts five `KEY=VALUE`
lines (in any order): `R2_ENDPOINT`, `R2_BUCKET`, `R2_REGION`,
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`. The bucket/region in that format
must match the backend manifest. Values are never echoed; do not use `source`
or export the file into the shell. Create files through your secret
reader/editor, not shell arguments/history. Never enable shell tracing. The
manifest contains only paths and public options, never keys. `~` expands on the
machine driving the command; other relative paths resolve beside the manifest.
The example manifest and commands cover all four configurations.
Missing credential files are still reported **skipped**
when deliberately running a subset or using a different operator machine.
Malformed/insecure existing files fail that backend, rather than being skipped.
Use unique names for class variants. Backend names scope object prefixes:
`cache-bench/<experiment>/<backend-name>/`. Do not reuse a nonempty prefix.

Both path and virtual-host addressing are supported upstream. A single backend
can be selected without a manifest:

```sh
python3 script/cache-bench/bench.py --run --placement A \
  --credentials "$HOME/.config/cache-bench/ovh" \
  --backend-name ovh-standard --bucket motor-cache-test --region de --addressing path --storage-class STANDARD \
  --prefix cache-bench/UNIQUE-RUN/ --output "$HOME/evidence/UNIQUE-RUN-A"
```

Omit `--storage-class` to use a provider's default. Credentials need scoped
ListBucket, GetObject, PutObject, HeadObject and DeleteObject permissions; OVH's
class audit can also use GetObjectAttributes. Do not change the manifest/name
between A and B. Nothing changes bucket defaults or lifecycle rules.

## Measurement protocol and output

For **each available backend**, A runs no cache, an independent remote fill,
three remote read-only runs, then a local-disk fill and local-warm best case.
B runs no cache plus three remote read-only runs and never fills. Every sample
removes `/srv/cache-bench/target`, then runs:

```sh
cargo build --locked --workspace
cargo test --no-run --locked --workspace
```

The second command reuses the first command's target *within* a sample, not
across samples. `CARGO_NET_OFFLINE=true`, incremental compilation is off, and
parallelism is fixed in the bundle. “Cold” means cold Cargo target, **not**
dropped OS page caches. No tests execute. Only motor is fetched; adding a larger
private fleet repository would need a separately pinned source/seed and access.
Use otherwise-idle hosts with comparable CPU/memory limits. Linking, uncacheable
work, disk/page caches and sandbox overhead remain placement differences.

Every backend gets its **own fill into its own prefix**, including the two OVH
classes. The gateway adds the configured `x-amz-storage-class` to fill PUTs and
signs it; no CopyObject/staging fallback is needed. Before reads, HEAD audits the
first 20 sorted real objects (or all if fewer). JSON records the actual returned
class header, size and ETag. For OVH only, a missing HEAD class header triggers
GetObjectAttributes(StorageClass), not an assumption of Standard. Other missing
class headers are reported as **not reported**. Requested-versus-returned class
mismatches/unknowns are visible as `class-mismatch`; measurements continue, but
the overall result is not a valid success. A backend ignoring a header is never
silently labelled High Performance.

Both placements also run, **for each available backend**:

* 200 sequential full GETs of real cache entries directly to the endpoint, and
  200 through the on-machine gateway. They cycle objects if fewer than 200 exist;
  `.sccache_check` is excluded. Persistent connections are reused. p50/p95 use
  nearest-rank end-to-end body-read durations, not a synthetic tiny-object RTT.
* A **30-second TCP/tiny-GET probe per route**. Each sample opens a new TCP
  connection, then a separate new HTTP(S) connection for a signed ranged GET of
  one byte from a real object. The gateway route also forces a new upstream TLS
  connection so its tiny-GET timings include the gateway's overhead fairly.
  TCP gateway means **loopback TCP**, not endpoint RTT. Tiny-GET timings include
  DNS/TLS/body read, while TCP timing includes DNS/connect only. The probe paces
  at most ten samples/second, reports failures/sample counts/actual duration,
  and can overrun 30 seconds by its final bounded network attempt.

JSON preserves raw sccache requests/hits/misses/cache errors, read/write errors,
read-hit/miss durations if reported, per-command exit/timing for completed
samples, byte/request counters, class audits and probe samples. If Cargo fails,
the error and prior completed samples survive; that sample's partial command
timings do not. No-cache stats are n/a, not artificial misses. Cache errors,
denied requests, zero-hit warm reads and RTT errors invalidate success but remain
visible. Missing-key backends alone do not invalidate an otherwise successful
subset. All-skipped runs fail rather than claiming useful measurements.

`results.md` is one table with **placement × backend** rows: no-cache/fill/three
RO/local-warm wall times, RO hit/miss/error and byte/request counters, direct and
gateway full-GET p50/p95, and both routes' TCP/tiny-GET p50/p95. Combine A and B
JSON as shown below for a single cross-placement table. Full per-backend JSON,
Markdown and owner-only Cargo logs are kept beneath each placement directory.
Wall time covers the two Cargo commands only; resets, daemon startup/shutdown,
HEAD/listing and probes are outside it. Bucket counters include daemon capability
checks/retries for a sample, but exclude audits/listing/probes. Bytes mean
**response bodies**, not TLS/header/wire bytes.

### Gateway and credential boundary

A loopback-only gateway on **each measured machine** re-signs requests and sends
verified HTTPS directly to that backend. It is not an off-host relay or HTTP
cache. It fences the exact prefix and independently denies writes during RO
samples. sccache sees dummy AWS credentials; real credentials stay in the
operator process/file and never enter Cargo/sccache env, source or job workspaces.
Error XML that can echo an access ID is stripped before it reaches sccache logs.
The driver strips inherited build/credential overrides and uses a dedicated Unix-domain socket (UDS)
daemon, restarted for every sample, not the host's regular daemon.

The Python gateway buffers objects and adds loopback/parsing/signing overhead.
Direct **and gateway** GET/RTT columns make that overhead visible, though
sequential probes also include network variation; do not treat their difference
as a perfect causal subtraction. The gateway supports the single-object
GET/HEAD/PUT pattern used by this pinned sccache/OpenDAL version, capped at 1 GiB
per object. Unexpected methods/paths/framing fail instead of bypassing metrics.

## Pinned source and provider findings

* OVH's [Endpoints and Object Storage geoavailability guide, “Mapping from AWS S3
  tiers to OVHcloud Storage tiers”](https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-location#mapping-from-aws-s3-tiers-to-ovhcloud-storage-tiers)
  lists Frankfurt `de` as **1-AZ**. On its `.io` endpoint, PUT
  `EXPRESS_ONEZONE` → **High Performance**, `STANDARD`/no header → **Standard**;
  GET/LIST/HEAD map the same names. Class is **per object**, not per bucket.
* sccache **v0.18.0** [`src/cache/s3.rs:22-128`](https://github.com/mozilla/sccache/blob/v0.18.0/src/cache/s3.rs#L22-L128)
  and [`src/config.rs:465-481`](https://github.com/mozilla/sccache/blob/v0.18.0/src/config.rs#L465-L481)
  have no storage-class field or setter. Unknown config fields are rejected.
  There is no `SCCACHE_S3_STORAGE_CLASS` option. Injecting the signed PUT header
  in this operator gateway is the small fallback that preserves independent fills.
* [`docs/S3.md:3-42`](https://github.com/mozilla/sccache/blob/v0.18.0/docs/S3.md#L3-L42)
  and [`src/config.rs:848-936`](https://github.com/mozilla/sccache/blob/v0.18.0/src/config.rs#L848-L936):
  supported names are `SCCACHE_BUCKET`, `SCCACHE_ENDPOINT`, `SCCACHE_REGION`,
  `SCCACHE_S3_USE_SSL`, `SCCACHE_S3_KEY_PREFIX`, `SCCACHE_S3_RW_MODE=READ_ONLY`.
  Credentials use `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, not invented
  sccache-prefixed key variables. Prefix trailing slashes are trimmed; invalid
  RW mode defaults to READ_WRITE, so spelling matters.
* [`src/cache/cache.rs:263-326,409-420`](https://github.com/mozilla/sccache/blob/v0.18.0/src/cache/cache.rs#L263-L420):
  all operations use the configured prefix root; RO checks read
  `.sccache_check` without a write probe, and RO writes are rejected before put.
  Misses compile but cannot populate the bucket; deferred write errors are
  separately counted ([server.rs:1646-1663](https://github.com/mozilla/sccache/blob/v0.18.0/src/server.rs#L1646-L1663)).
* [`s3.rs:87-158`](https://github.com/mozilla/sccache/blob/v0.18.0/src/cache/s3.rs#L87-L158)
  supports custom endpoint, region and virtual-host opt-in; SSL can override the
  endpoint scheme. OVH documents SigV4/S3 operations in its [compliance table](https://docs.ovhcloud.com/en/guides/storage-and-backup/object-storage/s3-s3-compliancy).
  These are source/doc compatibility findings, **not live handshake evidence**.
* [`compiler/rust.rs:1554-1597`](https://github.com/mozilla/sccache/blob/v0.18.0/src/compiler/rust.rs#L1554-L1597)
  hashes compile cwd and `CARGO_` values including `CARGO_HOME` (with explicit
  exclusions). A prepares source at `/srv/cache-bench/motor` and dependency seed
  at `/srv/cache-bench/cargo-home`; **B extracts A's exact bundle to those paths**.
  Target/TMPDIR/HOME paths are also fixed. Source/lock/driver digests, full commit,
  exact toolchain name, rustc -vV/sysroot, Cargo/sccache version and architecture
  must match. Toolchains must be installed at the same sysroot. The driver hash
  ensures A and B are not silently measuring with different script versions.
* [`server.rs:1673-1700,1809-1858`](https://github.com/mozilla/sccache/blob/v0.18.0/src/server.rs#L1673-L1858):
  stats JSON is ServerInfo.stats; `counts` and `adv_counts` classify the same
  events, so sum only `counts`. sccache has no bucket HTTP byte/request counters.
  The server's [0.18.0 local read-only-hit patch](../runner/patches/sccache-0.18.0-read-only-hits.patch)
  does not change S3 behavior; binary SHA is recorded separately from version.

## Exact operator sequence: A (direct Linux SSH shell)

Requirements: Python 3.10+, Git/tar, sccache 0.18.0, installed matching toolchain
and native compiler/linker prerequisites, enough disk for the seed/archive,
targets and a 50-GiB local-cache ceiling. Reserved `/srv/cache-bench` must be new
or empty. Create credential files and customize the copied backend manifest first.

```sh
set -eu
set +x
REPO="$HOME/motor"                    # contains the delivered scripts
PIN=$(git -C "$REPO" rev-parse HEAD)   # full commit, NOT /srv/cache-bench/motor
TOOLCHAIN=1.99.0                      # match the current prepared image/sysroot
EXPERIMENT="$(date -u +%Y%m%dT%H%M%SZ)-motor"
PREFIX="cache-bench/$EXPERIMENT/"
BACKENDS="$HOME/.config/cache-bench/backends.json"
EVIDENCE="$HOME/cache-bench-evidence/$EXPERIMENT"
SEED="$HOME/cache-bench-prefetch"
mkdir -p "$EVIDENCE" "$SEED"
chmod 700 "$EVIDENCE" "$SEED"

# Fetch outside the measurement, at the exact pinned revision.
git -C "$REPO" worktree add --detach "$EVIDENCE/fetch-source" "$PIN"
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  CARGO_HOME="$SEED" RUSTUP_TOOLCHAIN="$TOOLCHAIN" \
  RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= \
  cargo fetch --locked --manifest-path "$EVIDENCE/fetch-source/Cargo.toml"
git -C "$REPO" worktree remove "$EVIDENCE/fetch-source"
sudo install -d -m700 -o "$(id -u)" -g "$(id -g)" /srv/cache-bench
python3 "$REPO/script/cache-bench/bench.py" --prepare --source "$REPO" \
  --commit "$PIN" --cargo-home "$SEED" --toolchain "$TOOLCHAIN" --jobs 8 \
  --bundle "$EVIDENCE/assets.tar.gz" > "$EVIDENCE/preparation.json"
sha256sum "$EVIDENCE/assets.tar.gz" > "$EVIDENCE/assets.sha256"
printf '%s\n' "$PREFIX" > "$EVIDENCE/prefix.txt"
python3 "$REPO/script/cache-bench/bench.py" --run --placement A \
  --backends "$BACKENDS" --prefix "$PREFIX" --output "$EVIDENCE/A" \
  > "$EVIDENCE/A-stdout.json"
```

The `sudo install` grants access only to the reserved benchmark directory, not
`/srv` itself. Preparation refuses a nonempty experiment. The bundle includes
tracked source and registry/git seed directories, not original Cargo config,
standalone credential files, `bin/`, target or a checkout `.git`. Source/seed
metadata can still contain embedded credentials/private URLs: inspect and treat
the bundle as private. No private larger repository is included automatically.

## Exact operator sequence: B (operator machine drives Boat)

Copy A's bundle, independently recorded digest, prefix and A results to the
operator machine via SSH/scp. Use the **same driver revision** and backend names.
Credential-file paths can differ between machines, endpoints/options must not.
Missing key files remain skipped. Do not purge A's objects yet.

```sh
set -eu
set +x
REPO="$HOME/motor"
ASSETS="$HOME/cache-bench-input/assets.tar.gz"
ASSETS_SHA256='<paste SHA-256 recorded on A>'
PREFIX=$(< "$HOME/cache-bench-input/prefix.txt")
IMAGE='<current prepared named-snapshot handle>'
BACKENDS="$HOME/.config/cache-bench/backends.json"
OUT="$HOME/cache-bench-evidence/Boat-$(date -u +%Y%m%dT%H%M%SZ)"
# Replace <boat-key-reader> with a trusted local command printing the Boat key.
# The key is passed through stdin, never argv.
<boat-key-reader> | python3 "$REPO/script/boat-probe/probe.py" --key-stdin \
  --cache-bench --cache-image "$IMAGE" --size large --max-minutes 30 \
  --cache-bundle "$ASSETS" --cache-bundle-sha256 "$ASSETS_SHA256" \
  --cache-backends "$BACKENDS" --cache-prefix "$PREFIX" --output "$OUT"
python3 "$REPO/script/cache-bench/bench.py" --combine \
  "$HOME/cache-bench-input/A/results.json" "$OUT/cache-results/results.json" \
  --output "$HOME/cache-bench-evidence/combined-$(date -u +%Y%m%dT%H%M%SZ)"
```

The mode borrows the named snapshot unchanged, creates a `noEnv` clone (without
injecting account environment variables), waits
for hydration, authorizes a throwaway SSH public key and pins the API host key
with existing probe helpers. Uploads/execution use operator SSH, **not jobs**.
Each available key file streams over pinned SSH stdin into owner-only `/dev/shm`
files, removed in `finally`. Secrets never appear in API command bodies/journals,
image snapshots, persistent runner workspaces, source bundles or process argv.
The API key never enters the sandbox. Use only a trusted operator-owned image
with no untrusted concurrent workload. Shared probe cleanup attempts deletion of
the clone even when uploads/builds fail, never the borrowed image; exit 3 means
resources may still exist and require the cleanup retry below. Throwaway local SSH keys
are removed. The shared TTL is 30 minutes including upload/hydration/probes;
if it is too short the run fails/cleans up, not silently extending spend. Run a
smaller backend subset per clone if needed, using the same A-filled names/prefix.

Results are in `$OUT/results.json` (lifecycle plus nested matrix),
`$OUT/results.md`, and `$OUT/cache-results/results.{json,md}` (matrix-only).
Per-backend partial results are nested in the matrix if fetchable. B's detailed
Cargo logs stay in the disposable sandbox; A keeps its logs. Failed SSH streams
are not persisted because they may be sensitive. Inspect JSON status/skip/error
fields; a Markdown row is not by itself success evidence.

## Cleanup and interpretation

```sh
# Retry shared deletion after interruption, or after Boat's reported upload-URL expiry.
<boat-key-reader> | python3 "$REPO/script/boat-probe/probe.py" --key-stdin \
  --cleanup "$OUT/results.json"
# On A, AFTER B and evidence retrieval: deletes each configured available
# backend's prefix, including sentinels; verifies it is empty; reports counts.
python3 "$REPO/script/cache-bench/bench.py" --purge \
  --backends "$BACKENDS" --prefix "$PREFIX"
rm -rf -- /srv/cache-bench             # fixed-path assets/cache, NOT evidence
# Only after purge, remove each operator credential file when no longer needed.
```

Purge is explicit/repeatable, including after failed fills. It never deletes the
bucket or objects **outside** the configured prefixes. Use unique prefixes, do
not purge concurrently with another run, and retain keys until cleanup is done.
Use unversioned test buckets: versioning can retain noncurrent versions/delete
markers that require separate operator cleanup. Missing-key backends are skipped
by purge too, not falsely reported empty.

Choose an acceptable placement-slowdown budget before inspecting results.
Compare A/B RO medians/spread, hit/miss/error rates, uncached baselines and A's
local-warm floor; review actual returned classes and direct-versus-gateway costs.
No synthetic threshold declares production readiness. No production code paths
or runner credential filtering are changed.

## Linux fixture gates

```sh
python3 --version
python3 -m py_compile script/cache-bench/bench.py script/boat-probe/cache_bench.py script/boat-probe/probe.py
python3 -m unittest discover -s script/cache-bench -p 'test_*.py' -v
python3 -m unittest discover -s script/boat-probe -p 'test_*.py' -v
python3 scripts/run-script-tests.py
```

Workers run these with `runon: linux,8c`, never on the Mac. Fixtures do not compile
the workspace, contact providers, or assert live performance. Operator gates
above require direct host access, not the remote job runner.
