# Operator-only compile-cache placement benchmark

These scripts measure pinned Rust workspaces against several S3-compatible
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
dropped OS page caches. No tests execute. The legacy commands fetch motor only;
the hourly protocol below prepares separately pinned private fleet bundles.
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

## 72-hour interleaved OVH comparison

No live calibration or repository choice is checked in. **The first measurement
step is calibration on Boat large**, after preparing the three inputs. It measures
motor, prefrontal and broca in separate disposable sandboxes, using an uncached
build plus test compilation for each. `calibration.json` records cold Cargo wall
seconds and the raw dependency timing units from each command, plus lifecycle and
bundle evidence. Dependency unit-seconds are summed **non-workspace** Cargo unit
durations (including dependency build scripts); overlapping units are not CPU or
build wall seconds. The parser matches package name/version to full Cargo metadata
and rejects ambiguous matches. It parses the pinned Cargo version's HTML
`UNIT_DATA` (not a stable machine API); absent/changed timing data fails admission.

Selection admits only a candidate with more measured dependency unit-seconds than
motor. It picks the smallest estimated hourly budget that fits 1,800 seconds:
`7 × (motor cold seconds + candidate cold seconds) + 360s non-build allowance + 180s margin`.
Seven is six RO samples (three per class) plus the sixth-hour baseline. The 360s
allowance covers uploads/hydration, probes and cleanup; it is an estimate, **not a
guarantee**. A bad network/hour or large upload can exceed it. Runtime SSH steps
use the remaining TTL with a 30s cleanup reserve and fail rather than extending
the sandbox. If neither candidate fits, calibration fails and scheduling refuses
to start: inspect the evidence, do not hand-edit admission or invent a choice.

Each iteration uses **one new Boat large sandbox for both classes and both
repositories**, swapping bundles sequentially at the same absolute paths used
during fill. `/srv/cache-bench/motor` is deliberately the fixed checkout path even
for prefrontal/broca; `manifest.repository` identifies the real source. Three
cold-target RO rebuilds per class/repository, 200 full GETs per direct/gateway
route, and the existing TCP/tiny-GET probes run there. A single uncached baseline
per repository runs only at hours 6, 12, …, 72, before that hour's first class.
The server leg performs **only** direct TCP, tiny GET and 200 full GET probes per
class/repository, via pinned operator SSH. It does not extract sources or invoke
Cargo. Class order alternates for both legs; motor runs before the selected repo.
Within an iteration Boat precedes server, so the two placements are not simultaneous.

The scheduler fixes 72 slots, at start + 0h through start + 71h, not 72 hours of
continuous load. `state.json` is atomically checkpointed before/after each leg.
An exclusive output-folder lock prevents two schedulers sharing state. Restart
with **the identical command, script revision and inputs**: finished slots are
never rerun; an interrupted slot becomes failed; slots whose whole hour elapsed
offline become `missed` instead of being backfilled in a burst. The remaining
slots keep their original timestamps and class/baseline ordering. Failed Boat
legs do not suppress server legs or the next hour. Unresolved sandbox cleanup is
retried before another Boat clone; it can fail that Boat leg, not silently leak
additional clones. Keep the probe checkpoints for operator cleanup, even after
an interruption. Exit 1 at the end means at least one failed/missed slot, not that
the later hours were abandoned.

### 1. Prepare private inputs (direct Linux SSH host, never a runner job)

Use the **delivered script revision** throughout preparation, calibration, fill
and scheduling. Do not upgrade it midway. Start with empty reserved
`/srv/cache-bench`; this protocol replaces that directory between repositories.
Run the following in a direct operator SSH shell on the build server. Set the
three checkout paths and the exact toolchain installed in the prepared image.
Ensure native dependencies for all three workspaces exist in that image first.

```sh
set -eu
set +x
DRIVER="$HOME/motor/script/cache-bench/bench.py"
TOOLCHAIN=1.99.0
EXPERIMENT="$(date -u +%Y%m%dT%H%M%SZ)-ovh72"
ASSETS="$HOME/cache-bench-evidence/$EXPERIMENT/assets"
MOTOR_SOURCE="$HOME/motor"
PREFRONTAL_SOURCE="$HOME/prefrontal"
BROCA_SOURCE="$HOME/broca"
mkdir -p "$ASSETS"
chmod 700 "$ASSETS"
sudo install -d -m700 -o "$(id -u)" -g "$(id -g)" /srv/cache-bench
test -z "$(ls -A /srv/cache-bench)"
export PATH="/srv/build/cargo-home/bin:$PATH" RUSTUP_HOME=/home/ubuntu/.rustup
for NAME in motor prefrontal broca; do
  case "$NAME" in
    motor) SOURCE="$MOTOR_SOURCE";;
    prefrontal) SOURCE="$PREFRONTAL_SOURCE";;
    broca) SOURCE="$BROCA_SOURCE";;
  esac
  PIN=$(git -C "$SOURCE" rev-parse HEAD)
  SEED="$ASSETS/seed-$NAME"
  mkdir -m700 "$SEED"
  git -C "$SOURCE" worktree add --detach "$ASSETS/fetch-$NAME" "$PIN"
  env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
    CARGO_HOME="$SEED" RUSTUP_TOOLCHAIN="$TOOLCHAIN" RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= \
    cargo fetch --locked --manifest-path "$ASSETS/fetch-$NAME/Cargo.toml"
  git -C "$SOURCE" worktree remove "$ASSETS/fetch-$NAME"
  python3 "$DRIVER" --prepare --repository "$NAME" --source "$SOURCE" \
    --commit "$PIN" --cargo-home "$SEED" --toolchain "$TOOLCHAIN" --jobs 8 \
    --bundle "$ASSETS/$NAME.tar.gz" > "$ASSETS/$NAME-preparation.json"
  sha256sum "$ASSETS/$NAME.tar.gz" > "$ASSETS/$NAME.sha256"
  rm -rf -- /srv/cache-bench
  mkdir -m700 /srv/cache-bench
done
printf '%s\n' "$EXPERIMENT" > "$ASSETS/experiment.txt"
printf '%s\n' "$ASSETS" > "$ASSETS/server-assets.txt"
```

Use the actual `cargo_home` and `rustup_home` from `script/runner/toolchain.json`
if different from the example exports. Bundles include private tracked sources
and dependency seeds; inspect metadata for embedded secrets before use. Transfer
only with private SSH/scp, never public upload services or CI artifacts.

### 2. Calibrate first (operator machine)

Create a two-entry operator manifest containing **only** `ovh-standard` and
`ovh-high-performance` from `backends.example.json`. Both credential paths refer
to the operator's existing owner-only credential file. The scheduler refuses
missing keys, a third backend, different endpoints/regions, or missing classes.
The SSH alias below must have a pre-verified host key in `known_hosts` (no
`StrictHostKeyChecking=no`). Replace the key-reader example with your trusted
executable/arguments; use a wrapper executable if the reader needs a pipeline.
The command is parsed with `shlex`, not a shell. No keys belong in its arguments.

```sh
set -eu
set +x
REPO="$HOME/motor"
SERVER=cache-bench-server                  # your pinned operator SSH alias
REMOTE_ASSETS='/absolute/private/server/evidence/assets'  # ASSETS from step 1
INPUT="$HOME/cache-bench-input/ovh72"
OUT="$HOME/cache-bench-evidence/ovh72"
BACKENDS="$HOME/.config/cache-bench/ovh-pair.json"
IMAGE='<current-prepared-named-snapshot>'
KEY_READER='/absolute/path/to/boat-key-reader'
mkdir -p "$INPUT" "$OUT"
chmod 700 "$INPUT" "$OUT"
scp "$SERVER:$REMOTE_ASSETS/"'*.tar.gz' "$SERVER:$REMOTE_ASSETS/"'*.sha256' \
  "$SERVER:$REMOTE_ASSETS/experiment.txt" "$INPUT/"
python3 - "$INPUT" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
experiment = (root / 'experiment.txt').read_text().strip()
repos = []
for name in ('motor', 'prefrontal', 'broca'):
    path = root / (name + '.tar.gz')
    expected = (root / (name + '.sha256')).read_text().split()[0]
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    assert digest.hexdigest() == expected
    repos.append(dict(name=name, bundle=str(path), bundle_sha256=expected,
                      prefix='cache-bench/' + experiment + '-' + name + '/'))
(root / 'repositories.json').write_text(json.dumps(repos, indent=2) + '\n')
PY
python3 "$REPO/script/cache-bench/schedule.py" --calibrate \
  --repositories "$INPUT/repositories.json" --backends "$BACKENDS" \
  --image "$IMAGE" --key-reader "$KEY_READER" --output "$OUT/calibration"
python3 - "$OUT/calibration/calibration.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
print('Selected:', data['selected'], '\nReason:', data['reason'])
for name, result in data['records'].items():
    print(name, result['cold_seconds'], 'cold s;', result['dependency_unit_seconds'], 'dependency unit-s')
PY
```

Retain the complete calibration folder, not just its final JSON. A successful
calibration and matching raw results are mandatory before fill or scheduling.
No cache objects are read or written during calibration. Each calibration clone
is cleaned up by the normal probe path; the key reader runs afresh for each one.

### 3. Fill both classes once, from the server (operator machine drives SSH)

```sh
python3 "$REPO/script/cache-bench/schedule.py" --fill \
  --repositories "$INPUT/repositories.json" --backends "$BACKENDS" \
  --image "$IMAGE" --key-reader "$KEY_READER" --server "$SERVER" \
  --server-assets "$REMOTE_ASSETS" --calibration "$OUT/calibration/calibration.json" \
  --output "$OUT/fills"
```

This preflights **all four** selected repository/class prefixes before any fill.
A nonempty prefix refuses the whole operation. It verifies the independently
recorded bundle digests on the server, extracts each at the fixed paths, then
fills Standard and EXPRESS_ONEZONE independently and audits actual returned
classes. It never runs no-cache/local-cache/RO samples during fill. Fill Cargo
logs and results stay in the private server `$REMOTE_ASSETS/fills`; successful
matrix results are copied into `$OUT/fills` and checked against the pinned
bundles before scheduling. Failed fills must be explicitly purged, not resumed
or silently topped up. Preserve server evidence before using a new experiment.
The reserved root must already exist and be owned by your SSH user (step 1).

Object-store credentials stay in operator files and memory. Fill and network
legs stream parsed credentials **only via SSH stdin**, constructing S3 clients
in remote memory; no remote credential files, jobs, argv or evidence contain
them. Boat uses the existing owner-only `/dev/shm` credential files and removes
them in `finally`. The API key is supplied via `probe.py --key-stdin`, never an
environment variable or command argument. Do not enable shell tracing.

### 4. Start/resume, check progress, summarize (operator machine)

```sh
mkdir -p "$OUT/schedule"
chmod 700 "$OUT/schedule"
nohup python3 -u "$REPO/script/cache-bench/schedule.py" \
  --repositories "$INPUT/repositories.json" --backends "$BACKENDS" \
  --image "$IMAGE" --key-reader "$KEY_READER" --server "$SERVER" \
  --calibration "$OUT/calibration/calibration.json" --fills "$OUT/fills" \
  --output "$OUT/schedule" > "$OUT/schedule.log" 2>&1 < /dev/null &
echo $! > "$OUT/scheduler.pid"

python3 - "$OUT/schedule/state.json" <<'PY'
import collections, json, sys
data = json.load(open(sys.argv[1]))
print(collections.Counter(h['status'] for h in data['iterations']))
for h in data['iterations']:
    if h['status'] != 'pending':
        print(h['hour'], h['status'], h.get('order'), 'baseline:', h['baseline'])
PY
tail -n 20 "$OUT/schedule.log"
# Safe during the run: the state snapshot points only at completed report files.
python3 "$REPO/script/cache-bench/bench.py" --summarize "$OUT/schedule"
```

Rerun the identical `nohup` command after a restart (check no scheduler still
owns the folder). The operator machine must stay awake/online; the key reader
must work noninteractively for three days. Do not delete input bundles,
calibration, fill evidence, or change credential paths while resuming. Keys can
be rotated in their existing files. The scheduler fingerprints scripts/options,
not credential values.

`summary.{json,md}` pools raw successful full-fetch/TCP/tiny-GET latency samples
per class × placement × repository × route, reporting nearest-rank p50/p95/p99,
error/timeout denominators, rebuild/baseline distributions and hit/miss/cache
error counts. It includes all schedule slots (including failed/missed ones),
hourly measurements and HP-minus-Standard paired differences for each measured
hour/metric. Missing observations are never imputed. Errors/timeouts are logical
probe outcomes; a stale-connection retry that succeeds is included in the
end-to-end latency, not counted as a failed logical fetch. A deterministic
descriptive comparison states whether the absolute median paired difference
exceeds the paired-hour IQR (at least six pairs required); it makes no statistical
significance, causal, or production recommendation. Network-only server rows have
no rebuild distribution, not artificial zero-second builds.

### 5. Cleanup/purge (after schedule stops and evidence is retained)

```sh
# For any checkpoint reporting incomplete cleanup (also after an interrupted hour):
# YOUR_BOAT_KEY_READER | python3 "$REPO/script/boat-probe/probe.py" --key-stdin \
#   --cleanup "$OUT/schedule/hour-NNN/boat/probe/results.json"
python3 - "$OUT/calibration/calibration.json" > "$OUT/purge-prefixes.txt" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
for repo in data['repositories']:
    if repo['name'] in ('motor', data['selected']):
        print(repo['prefix'])
PY
while IFS= read -r PREFIX; do
  python3 "$REPO/script/cache-bench/bench.py" --purge --backends "$BACKENDS" --prefix "$PREFIX"
done < "$OUT/purge-prefixes.txt"
# On the direct SSH server, after saving its fill logs:
ssh "$SERVER" 'rm -rf -- /srv/cache-bench'
```

Purge runs on the operator machine using its local credential files and verified
HTTPS; it verifies each class prefix is empty. Calibration never populated the
unselected repository. After a partial failed fill, purge the same selected
prefixes before preparing a **new** experiment. Do not purge during reads. The
legacy unversioned-bucket warning below still applies.

## Legacy exact operator sequence: A (direct Linux SSH shell)

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

## Legacy exact operator sequence: B (operator machine drives Boat)

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
python3 -m py_compile script/cache-bench/bench.py script/cache-bench/schedule.py script/boat-probe/cache_bench.py script/boat-probe/probe.py
python3 -m unittest discover -s script/cache-bench -p 'test_*.py' -v
python3 -m unittest discover -s script/boat-probe -p 'test_*.py' -v
python3 scripts/run-script-tests.py
```

Editing workers run these on the remote Linux test runner with four allocated
vCPUs (`runon: linux,4c` in the worker shell tool), not on the shared Mac. Fixtures do not compile
the workspace, contact providers, or assert live performance. Operator gates
above require direct host access, not the remote job runner.
