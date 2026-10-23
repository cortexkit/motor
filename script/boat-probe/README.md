# Boat spill-provider environment probe

The runner-host lane also checks Boat's per-job PID bootstrap on systemd 255.
`pid_namespace.py` collects evidence in the real job and in an independent
`User=motor-job` reference unit. Check names include `boat_pid_profile`,
`pid1_cgroup_readable`, `pid1_same_cgroup`, `pid1_is_wrapper`, `job_pid_is_small`,
`outside_pids_esrch_or_proven_alias`, `job_credentials_match_systemd`,
`all_job_capabilities_empty`, `job_no_new_privileges`, `job_namespace_filter`,
`proc_protections_match_systemd`, `job_signal_state_matches_systemd` and
`session_keyring_owned_by_job`. The existing proc/ps checks still require only
job processes. Known runner, endpoint, sshd and provider PID numbers must yield
ESRCH with no proc entry, not the old shared-PID-namespace EPERM result, unless
the number aliases a proven job-local process. The host records UID, cgroup,
comm and stat starttime; an alias must have the job's UID/cgroup and a different
starttime from that host identity. Sandbox PID 1 is intentionally such an alias
for the job's own bash wrapper. Aliases are explicitly recorded, not skipped.

Guest errno values are Linux ABI values even when the operator runs the probe
on macOS (Linux ENOSYS is 38, not macOS's 78). Lifecycle checks use the wire
protocol's `outcome.type`: a command killed beneath the wrapper yields an
`exit` with code137, while SIGKILL of the unit yields `signal` with signal9 and
the client's normalized code137. Those outcomes are checked separately, not
accepted interchangeably. `fixtures/pid-live-initial.json` projects real job
results covering both cases and the cross-platform errno comparison.

New runner-host FAIL records include a `reason`, rendered under failed-check
reasons in the Markdown report. Lifecycle and namespace-filter reasons include
the expected and observed terminal or syscall values. Rendering historical
evidence preserves its recorded verdict rather than recomputing a pass under
newer rules.

`job_clone3_pid_denied` requires clone3 with CLONE_NEWPID to return ENOSYS;
`job_inherited_fds` records the actual initial job fd inventory and allows only
0/1/2. For each setsid teardown job on old systemd, the root observer additionally
checks `post_drop_pdeathsig`, `private_session_and_pgid`, `bootstrap_exec_fds`
and `bootstrap_expected_submounts`. Those checks read the root-owned 0600
pre-exec telemetry (not job-authored claims) and compare the actual wrapper and
supervisor sessions in the host's view. Expected mounts come from the live
independent reference unit, not the fixture's illustrative path list.

Additional production-client jobs check `job_stdin_eof`, `sigkill_job_exit_137` and
`pid1_ignores_job_sigkill`. A root observer stops or SIGKILLs a unit while its
client is waiting and verifies `setsid_stop_reaped`, `setsid_kill_reaped` and
the corresponding `setsid_stop_exit_143` / `setsid_kill_exit_137`. The escapee
ignores TERM and has its own session; the observer must find it before acting,
check `KillMode=control-group`, and see no survivor after teardown. These are
live operator checks, not claims made by fixture tests. Offline regression
fixtures are in `fixtures/pid-namespace.json`; run
`python3 -m unittest discover -s script/boat-probe -p 'test_*.py' -v`.

Operator-only, disposable measurement harness. Python 3.10+ and its standard
library are the only local requirements. Portable tests prove parsing and cleanup
plumbing, **not** the behavior of a Boat sandbox or motor-runner.

From the repository root, with a key reader that emits **only** the Boat API key:

```sh
set -o pipefail
<key reader> | python3 script/boat-probe/probe.py --key-stdin \
  --output .cortexkit/internal-docs/research/boat-live --max-minutes 30
```

Alternatively set `BOAT_API_KEY` yourself and omit `--key-stdin`. Never put the
key in argv, a committed file, a log or a shell with `set -x`. The harness removes
the environment variable after reading it, never sends it into the sandbox and
redacts it from API errors and saved output. The key needs sandbox create, read,
exec, stop, resume, **delete/delete-own**, snapshot read/write, and provider usage
access. An archive/stop-only key is not sufficient. No vault enrollment is created
by this probe; the operator supplies their existing authorized reader.

### Sandbox size (`--size`)

`--size large|default|small` (default `large`) is the Boat type of **every** sandbox a
run creates, including the `--verify-prepared-image` and `--from-prepared-image`
clones. Boat publishes large as 8 vCPU / 16 GB, default as 4 vCPU / 8 GB and small
as 2 vCPU / 4 GB (<https://docs.boat.dev/machines>); the probe checks that Boat
reports the requested size, but every job limit comes from what the sandbox
measured. A runner-host run records, per size and phase, under `sizing.<size>` in
`results.json` and the "Sandbox sizing" table in `results.md`: usable vCPUs
(`nproc`), the runner's `--max-weight`/`--rustc-slots`, the endpoint's
`vcpu_ceiling`, MemTotal, MemAvailable before the runner started, and the job
`memory.max` the runner was given. Probe jobs ask for the sandbox's measured vCPUs
(the client's optional fifth argument); `isolation.cpu_limit` expects `cpu.max`
for that request and `nproc` equal to the measured count. `runner_vcpus_follow_sandbox`
fails when the runner or endpoint still uses another size's CPU count (an image
recipe from before the start wrapper measured CPUs bakes in the build sandbox's
count), and `runner_memory_from_measurement` requires the runner's
`--boat-memory-max-bytes` to equal the budget measured in that sandbox. A job
listed in `runner_host.JOB_VCPUS` with more vCPUs than the sandbox has is skipped
and recorded under `phases[].skipped_jobs` with the reason, not failed; no current
job has a fixed request.

Every prepared image so far was saved on `large`. When Boat refuses to create a
smaller size from such a snapshot (any 4xx except 401/404/408/429, a 403 naming
`too_small`, or a sandbox that fails while restoring with `too_small`), the run
records `snapshot_size_bound` with Boat's HTTP status and error text, the verdict
outcome is `snapshot_size_bound` and the exit status is 4, instead of a generic
failure. Qualify the prepared image on `default`:

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin --runner-host \
  --verify-prepared-image motor-boat-prepared-09f9365aa9a8 --size default \
  --cargo-seed-manifest /secure-input/cargo-seed.json --cargo-seed-sha256 "$CARGO_SEED_SHA256" \
  --acknowledge-tenant-one-private-cache \
  --output .cortexkit/internal-docs/research/boat-default-verify --max-minutes 30
```

### Runner host prerequisites (no installs)

For the full toolchain recipe, real motor-client jobs and named-snapshot timing,
see [`script/boat-image/README.md`](../boat-image/README.md) and use `--runner-host`.
That mode keeps direct real-job checks on probe-only loopback port 19443, and
also replays the broker's identity install and readiness-marker read to start
the authenticated endpoint on 127.0.0.1:9443 in each order the broker can
produce, then verifies invalid-token refusal and root-private tenant sockets.

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin --runner-prerequisites \
  --output .cortexkit/internal-docs/research/boat-runner-prerequisites --max-minutes 5
```

Allow about **one minute**, plus cleanup. Creates one sandbox of `--size` (default `large`), records
`/etc/os-release`, systemd version and an actual `PrivatePIDs=yes` transient
unit, effective SSH policy/configuration, and root open plus
`KVM_GET_API_VERSION` on `/dev/kvm`. Nested-module flags are recorded separately;
an API version alone does not prove a nested guest boots. No VM is created, no
tools are installed, and PID 1 and sshd are never replaced. Unsupported
`PrivatePIDs` is retained as raw evidence, not hidden by dropping the property.
Creation uses `noEnv: true` so Boat attaches no account environment, secrets or
credentials. An account-credential audit enumerates sensitive
environment variable names and credential paths in `/root`, `/home/user` and
`/etc`, without logging their values or contents; scan errors prevent a clean
audit. This is a bounded detection check, not proof that arbitrary hidden secrets
cannot exist. The ordinary cleanup/recovery contract below applies. Prerequisite success means
evidence was collected, not that all runner capabilities passed.

### SSH reachability from an IPv4-only operator machine

`--ssh-reach` is a separate focused mode; it also requires local `ssh` and
`ssh-keygen`, and the Boat key's `ssh` action:

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin --ssh-reach \
  --output .cortexkit/internal-docs/research/boat-ssh --max-minutes 10
```

It creates one fresh sandbox of `--size` (default `large`), generates throwaway Ed25519 tested/witness keys in
an owner-only temporary directory, and authorizes only their public keys through
`POST /sandboxes/{id}/sshkey`. It records `sshEndpoint` versus `machineIp`, prefers
the returned endpoint, and forces an IPv4 SSH connection. Known-hosts contains
only the API-returned host key. `StrictHostKeyChecking=yes`, a private known-hosts
file and alias, no global known-hosts, no DNS trust, no updates, no agent, no
password fallback and no multiplexing prevent TOFU or an old session from masking
a failure.

The mode separately measures an SSH command and a **loopback-only** TLS echo
service reached through `ssh -N -L` to `127.0.0.1:9443`. It does not open 9443 in
UFW or expose the TLS service publicly. It stops/resumes twice, records host-key
and endpoint changes, tests the old private key before reauthorizing that tested
key, then writes exactly
`restrict,port-forwarding,permitopen="127.0.0.1:9443",command="/bin/false"`
on its authorized_keys line. The restricted phase separately checks refused exec,
shells with and without a PTY, permitted TLS forwarding, a forward to a second
loopback listener, and remote forwarding. It then stops/resumes, re-reads the
tested line byte for byte without reauthorizing that key, and repeats those checks.

Both shell tests require an authenticated connection and no marker output; the
PTY-requesting test separates a refused shell from merely disabled PTY allocation.
Restriction persistence compares the exact tested line, including its newline,
and reports normalization/whitespace changes as FAIL with the raw before/after
lines for inspection. The separate-port and remote-forward checks require an SSH
server refusal, not just a failed TLS attempt or an unreachable endpoint.

A different throwaway witness public key obtains the refreshed host key after
resume. Reinstalling the tested key before checking persistence could normalize
its options and invalidate the test, so that never happens during the restricted
resume check. The script preserves all unrelated authorized_keys lines and
collapses duplicate lines only for its own tested key. The private keys are never
logged and are removed locally; each local SSH tunnel is terminated and reaped.

Every check has PASS/FAIL plus raw evidence under `ssh_reach`. An authentication
or route failure does not count as successful shell refusal. OpenSSH's `restrict`
option does not itself forbid command execution, so this mode tests the requested
forced-command line instead of assuming it creates a forwarding-only login.
Negative capability findings are FAIL, not a rewritten expectation.
Allow roughly **3–8 large-sandbox minutes**, plus asynchronous purge latency; this
is an estimate, not a measured SSH result. No toolchain or named snapshot is
created in this mode. Boat SSH reference: <https://docs.boat.dev/ssh-access>.

The output directory must not exist. It contains an owner-only `results.json`
checkpoint and a rendered `results.md`; the same JSON is printed to stdout after
cleanup. Keep evidence under ignored `.cortexkit/internal-docs/`, outside build
directories. The Markdown template is embedded in `probe.py` (`TEMPLATE`) so the
renderer works without private task documents. Re-render without a credential:

```sh
python3 script/boat-probe/probe.py \
  --render .cortexkit/internal-docs/research/boat-live/results.json
# Export the unfilled Markdown template instead:
python3 script/boat-probe/probe.py \
  --write-template .cortexkit/internal-docs/research/boat-findings-template.md
```

## What it measures

- One fresh sandbox of `--size` (default `large`, provider-reported 8 vCPU, 16 GB), no inherited account
  environment/logins (`noEnv: true`). Records raw uid, sudo, process capabilities,
  kernel, PID 1, systemctl, transient systemd unit, cgroup controllers and child
  cgroup writes; each namespace separately with noninteractive sudo if available;
  KVM presence, filesystems, capacity and a real Linux FICLONE ioctl.
- A TLS echo listener on TCP 9443 using a guest-generated certificate trusted by
  the outside client, not Boat's HTTPS proxy or SSH endpoint. Tests the public IP
  before and after the documented guest `sudo ufw allow 9443/tcp`, with raw
  `iptables`/`ip6tables` rule dumps. Only this port is opened; Boat's control ports
  and outbound policy are not modified. Records DNS
  and direct outbound HTTPS to example.com. IPv6-only sandboxes require IPv6 on
  the operator machine; an unreachable client route is not firewall evidence.
- Three complete stop/resume cycles, provider-state timing, command-ready timing,
  median, IP pairs, file-marker hashes, loopback listener readiness, firewall dumps
  and external TLS echo **before** reapplying each resume's rules.
- Three real Rust default-profile toolchains (1.85.1, 1.86.0, 1.87.0) in the
  snapshot-persisted home directory; allocated bytes and install/version outputs.
  The result flags whether at least 2 GiB really installed; no sparse-file padding
  substitutes for installed tools. This deliberately downloads rustup and official
  Rust distributions in a disposable guest, not a production image build recipe.
- Named snapshot save through ready, stop original, create a second sandbox of the same size from
  that snapshot, check marker/toolchain availability, then one loaded stop/resume.
  Ready timings do not imply all files hydrated; toolchain-ready timings execute
  the installed compilers after waiting for full hydration. Marker checks also
  wait for hydration, rather than treating a file still restoring as data loss.
  No motor-runner or broker is installed by this probe;
  it does not establish runner token enforcement or end-to-end job correctness.
- Provider usage via `GET /sandboxes/{id}/usage` before deletion. Usage is never
  inferred from guest clocks. Raw API responses and timings are retained.

Allow roughly **15–30 large-sandbox minutes**, plus a few minutes of cleanup;
installation and snapshot speed are unmeasured until the operator runs it. The
original stops before the clone starts, rather than billing both in parallel.
`--max-minutes` limits the measurement wall-clock budget, not provider billing or
cleanup duration. API calls have 40-second timeouts. An 1800-second auto-stop TTL
is a backstop, not deletion (Boat may extend TTL while work is active). Actual
charges come from Boat's returned usage; named-snapshot storage can be separately
charged if the account is above its free allowance.

Guest commands are detached: Boat gives them no server-side time limit. The
poll budget is client-side; on timeout the harness enters resource deletion.
Individual guest subprocesses also have bounded timeouts. Command output is
limited to the API's maximum 8 MiB tail; truncation fails visibly instead of
silently parsing incomplete measurements.

## Cleanup and interruption recovery

`finally` handles errors, Ctrl-C and SIGTERM and attempts **every** resource's
deletion. A second signal is ignored during cleanup; SIGKILL, host loss or a
revoked/insufficient key still requires recovery. IDs are checkpointed before
probes. A lost create response is recovered using the same documented
`Idempotency-Key` and request body (valid for 24 hours; the script refuses replay
after 23 hours). Snapshot names are unique and checkpointed before saving.

Sandbox deletion sends the required `X-Ascii-Confirm-Delete` header and polls the
returned deletion operation. A 202 or an archived/hidden sandbox is **not** marked
fully purged (`cleanup_complete` stays false). Missing delete scope, a deletion
that Boat never accepted or a snapshot save still in flight is reported as
`cleanup_state: "incomplete"`. A deletion operation
at `waiting_for_uploads` means deletion was authorized and accepted, but stored
data awaits expiry of existing upload URLs (`expectedBy`); a different key scope
does not accelerate it. The harness reports this as `cleanup_state:
"purge_pending"` without several minutes of redundant polling. It attempts
fallback stop only when no deletion operation was accepted. Retry after the
operation's reported `expectedBy` to confirm the purge, or immediately after
correcting an actual permission error:

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin \
  --cleanup .cortexkit/internal-docs/research/boat-live/results.json
```

Recovery operates only on the checkpoint's recorded IDs/names. Treat checkpoint
files as trusted input, not files obtained from an untrusted sandbox. A completed
cleanup reports each resource as deleted and `cleanup_complete: true`.

The exit status reports the measurement verdict, and cleanup separately:

| Exit | Meaning |
| --- | --- |
| 0 | Verdict passed (JSON `verdict.passed`); cleanup is `complete` or `purge_pending` |
| 1 | Verdict failed: measurements incomplete, an error, or a required check did not pass (`verdict.reasons`) |
| 3 | `cleanup_state: "incomplete"`: resources may still exist; retry `--cleanup` (checked before the verdict) |
| 4 | `verdict.outcome: "snapshot_size_bound"`: Boat refused to create the requested `--size` from a snapshot saved on a larger size |

`--cleanup` exits 0 unless cleanup is still incomplete. A Boat purge pending after
accepted deletes never fails a run: nothing is left running and retrying cannot
speed it up.

`verdict.inconclusive` lists provider idle-reclaim checks marked `INCONCLUSIVE`. They
remain `INCONCLUSIVE` in the check list rather than counting as passes, but do
not block the runner-host verdict. Every parity, storage and other required check
still needs `PASS`; a failed required check cannot be hidden by an inconclusive
reclaim observation.

`Error: none recorded` in rendered findings means no measurement exception was
caught. It does not override `cleanup_complete: false` or a failed network probe.
A `--runner-host` checkpoint renders a runner-host report instead: verdict and
failing check names, per-phase job codes and cgroup limits, install/snapshot
timings, the per-phase job memory budget, snapshots and cleanup.
Older output used `Error: "unmeasured"` for the same absent JSON `error` field;
that was a rendering placeholder, not a provider error or an unrun measurement.

## Portable verification

```sh
python3 --version
python3 -m unittest -v script/boat-probe/test_probe.py
python3 scripts/run-script-tests.py
```

API shapes follow [v1](https://docs.boat.dev/api/v1), including the linked stop,
resume, named-snapshot, sandbox-usage and irreversible-delete references. Raw
networking follows [Public URLs / raw TCP](https://docs.boat.dev/hosting) and
[Firewall](https://docs.boat.dev/hosting/firewall): guest root rules, not an
invented provider firewall API. No live provider assertion follows from these
portable tests.
## Disk-quota qualification

The runner-host probe fills the real total quota (up to 32 GiB, with nonzero
random data and fsync) before its stop/resume cycle. It
first tries native and compat attribute setters, requires EPERM, checks that a
new file still inherits the job project, then fills that same directory. A
preallocated barrier lets the root observer measure **both** root and XFS free
space while the failed write's data is still present. A pre-fill handshake
captures physical headroom before writes begin. Ordinary lanes require the
root-read kernel hard limit and near-limit charge, EDQUOT/ENOSPC, a normal failed
terminal, cleanup/readiness, and a normal next job. Netlink warnings are recorded
but are not a substitute for those checks: multicast loss must not fake either
success or failure. Probe results retain `runner_host.disk_quota`,
including `at_limit.root_free_bytes` and `at_limit.xfs_free_bytes`, so the
provisional 24-GiB reserve can be tuned empirically. `reserve_at_limit` proves
only enforcement of that floor, **not its sufficiency**; `reserve_assurance`
explicitly leaves `sufficiency_proven` false.

Separate jobs exhaust workspace, target, scratch, HOME and `/var/tmp`, and allocate past the same
limit on the existing root-created `/motor-output/stdout` inode. The stdout lane
uses `fallocate(KEEP_SIZE)`: it must allocate nearly the entire allowance before
EDQUOT/ENOSPC and a matching near-full kernel quota, while preserving the root-disk
reserve. It verifies `st_blocks` as well as project ID, so a refused no-op is not
a pass. Keeping the file size at zero avoids streaming tens of GiB into the
operator client; the lane punches its unwritten extents before reporting. Every
lane checks project IDs for workspace, target, HOME, `/tmp`, `/var/tmp`, stdout,
stderr, exit and cache-stats. Only quota jobs request the operator example's
optional bounded 900-second deadline; existing probe invocations still default
to 120 seconds. `phases[].kernel_release` records `uname -r`.

A 64-MiB retained-target baseline must be relabelled and reduce the successor's
remaining growth. Another job leaves actual FICLONE copies; its successor
deletes copies while retaining their source, then fills with unique data. The
probe checks initial logical charge, reduced growth, per-file allocation and
physical free-space deltas against an independently sampled cap, rather than
allowing initial-plus-growth credit. A 2,200-level private tree must finish
without stranding a lease and leave capacity for the next job. Stale cleanup
ordering is covered deterministically through the real Rust lease/lock code
with kernel calls seamed, in both orders and across restart. No privileged
release-only probe entry point is shipped; live forced stale cleanup is skipped.

Finally a quota-sized **target file stays on disk**. The job remains at its
barrier so idle eviction cannot remove it before the provider stops/snapshots
the sandbox. The client is intentionally interrupted for this final snapshot
case, not reported as a normal quota terminal; preceding lanes prove those
terminals. Stop must archive successfully, resume must hydrate, and the resumed
phase must run a normal job. This is not merely a stop after unlinking the fill.
Failure cleanup stops writers and removes only the disposable probe's fills,
so a failed reserve qualification does not deliberately leave the sandbox full.

Before the jobs, an unfiltered root control on the same XFS mount sets and clears
project inheritance with FSSETXATTR and verifies the actual flag transition. The
job must get EPERM for that same ioctl. Root also invokes native syscall 469
(`file_setattr`) with a 24-byte `file_attr`, an empty pathname and `AT_EMPTY_PATH`
to clear the flag; the real job requires ENOSYS from the
filter before filling its inherited directory. If root can use the syscall, the
job refusal is a positive PASS. If root gets ENOSYS (for example on a pre-6.17
kernel), that case is explicitly **INCONCLUSIVE**, not PASS: because root also
returned ENOSYS, the case cannot establish that the job's ENOSYS came from its
syscall filter. The result records the
root control, errno and kernel release. The probe still collects quota and
stop/resume evidence, but does not claim every required check passed when the
path-setter case is inconclusive.

Run on the largest deployed size (`xlarge`) against a **newly rebuilt** prepared
image. Use the existing credential reader and seed arguments:

```sh
set -o pipefail
<key reader> | python3.14 script/boat-probe/probe.py --key-stdin --runner-host \
  --size xlarge --verify-prepared-image <new-prepared-image> \
  --cargo-seed-manifest <seed.json> --cargo-seed-archive <seed.tar.gz> \
  --cargo-seed-sha256 <manifest-sha256> --acknowledge-tenant-one-private-cache \
  --max-minutes 30 --output .cortexkit/internal-docs/research/boat-disk-quota
```

This is a real disk workload, not a portable unit test. A root-space emergency
watchdog kills the probe job if containment fails and either filesystem falls more
than 64 MiB below the provisional reserve. Such a run is a qualification failure,
not a successful quota result. Do not qualify an older image without the new
quota and syscall filter. No live qualification is implied by green unit tests.
