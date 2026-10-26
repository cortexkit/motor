# motor-vm-helper

`motor-vm-helper` is a Linux root service that owns a pool of Firecracker VM
slots and hands the unprivileged runner only file descriptors. No runner
backend uses it in this build: installing it does not change how any runner
instance runs jobs.

[docs/linux-microvm.md](../../docs/linux-microvm.md#motor-vm-helper) describes
its host prerequisites, installation, protocol, restart behavior and tests.
The helper's fixed storage root is `/var/lib/motor-vm`, the root-trusted bind
view of `/srv/build/vm`. Install and enable the versioned
`systemd/motor-vm-storage.mount` unit (installed as `var-lib-motor\x2dvm.mount`,
the name systemd requires for that path) and
`systemd/motor-vm-storage-private.service` companion before starting the helper.
The companion runs `mount --make-rprivate` after mounting, so mounts cannot
propagate back through the source alias; the helper requires it. The install
sequence is in the host prerequisites linked above. Root-owned data below an
unprivileged-owned parent is not sufficient: all VM reads use the protected
mounted path.

# Persistent tenant operations

The existing connection-owned job slots are unchanged. Tenant operations are
independent of job leases and survive client disconnects. Authorized peers can
send `ensure`, `attach_disk`, `suspend`, `restore`, `stop_tenant`, `set_cpu_max`,
`set_egress`, `open_relay`, `adopt`, `tenant_metrics` and `recovery_complete`.
Each takes only `tenant`; `set_cpu_max`
also requires integer `vcpus` (1 through the registry ceiling), and `open_relay`
requires `port` (52, 53 or 54). Hyphenated aliases are accepted for the multiword
ops. Unknown fields are refused. `open_relay` returns one SCM_RIGHTS descriptor
with `{"result":"relay","fd_count":1}`; other tenant operations return
`{"result":"tenant","running":true,"restored":false,"generation":1}`.
`adopt` only observes the current helper's VM, and never wakes a stopped VM.
`tenant_metrics` returns `memory_bytes` from its VM cgroup's `memory.current`
and `egress_bytes` from its own fixed forward-rule counter, never host-input or
NAT counters. It changes nothing and accepts no caller-selected path or handle.

Only root provisioning writes `/etc/motor-vm-helper/tenants/<tenant>.json`.
Tenant names are validated before opening any registry path, and registry
symlinks/writable ancestors are refused. See `script/tenant-provision/README.md`
for the registry, image, cgroup and disk prerequisites. Tenant VMMs specifically
use `/usr/local/lib/firecracker/1.17.0/{jailer,firecracker}`; existing job binaries
remain configured as before. They use ordinary file-backed RAM, no job UFFD
loader, and never clone a tenant disk. A disk lock prevents duplicate attachment.

Fresh starts load a paused base snapshot for the pinned image, vCPU ceiling and
guest RAM size, update the drive and tap,
and durably increment generation before resuming. Suspend requires a successful
guest `prepare_snapshot` (bounded drain, sync and freeze), snapshots to a key
containing image hashes and generation, then stops the VMM. Insufficient byte
space returns `suspend_skipped_space`. Failed preparation or snapshot creation
leaves the VM running and thaws the guest. `stop_tenant` tries disk preparation
for at most 15 seconds, then stops without a snapshot even if the guest refuses
or is unreachable. Before an unclean stop it fsyncs an `unclean-stop.json` record
beside the disk's generation file, including time and reason. Restore consumes/fsyncs the snapshot and advances
generation **before** resume; it then thaws. The host still calls `post_restore`
on a restored VM and `attach_tenant` on a fresh one. Mismatched keys or failed
snapshot loads fall back to the size base, preserving the disk. A helper restart
kills old tenant VMMs, but never removes tenant disks or generation files.
Such kills also mark the disk unclean. An unclean disk always takes a fresh
start, whose helper reply includes `recovery_required: true`. The host passes
that requirement to guest initialization and does not relay work until it
succeeds. Recovery runs inside the guest only: mount for XFS journal replay,
unmount, `xfs_repair -n`, remount, then Git integrity checks before runner start.
`recovery_complete` checks the guest's successful recovery generation before
durably removing the host flag. Failure leaves it set. Clean starts are unchanged.

Unclean stops are detected even when nothing ran to record them. Just before a
tenant VMM starts, the helper fsyncs a `running.json` marker beside the
generation file. Only a `stop_tenant` whose guest preparation succeeded, or a
completed suspend, removes it. Any later request for a tenant whose VM is not
running but whose marker is present first writes `unclean-stop.json` (reason
`vm_stopped_without_clean_stop`). That covers a helper killed by SIGKILL, a
host power loss and a VMM that died on its own. A root-owned marker written by
hand (`{}` is enough) makes the next start run recovery the same way.

Stopping the service is a clean stop. The unit uses `KillMode=mixed`, so
SIGTERM reaches only the helper, not the tenant VMMs in its cgroup. The
helper takes SIGTERM and SIGINT on one thread and, once any request in
progress finishes, runs `stop_tenant` for every running tenant, refuses all
later requests and exits. A tenant whose preparation fails is recorded as
unclean before its VMM is killed. Each tenant's outcome and stop time go to
the journal. systemd then SIGKILLs what is left (job VMs). `TimeoutStopSec`
covers one in-flight suspend and one tenant's stop; the unit file shows the
arithmetic. Tests: `service_stop_signal_runs_the_clean_tenant_stop_before_the_helper_exits`,
`service_stop_signal_records_a_tenant_whose_guest_flush_fails_as_unclean`,
`killed_helper_leaves_the_running_marker_so_the_next_start_runs_recovery`.

The host's `motor_tenant` nftables table is independent of the guest's per-job
`motor_net` table. On a shared build server it also cooperates with the host's
`motor_net`: only an exact helper-maintained `tenant_taps` set escapes that
table's unrelated-forwarding drop. The versioned set is persisted empty; the
helper and installed loader serialize runtime delegation with a private network
lock. No wildcard TAP allowance bypasses the build-server job rules.

The tenant policy explicitly accepts validated public IPv4 egress and established
return traffic, then drops unmatched TAP traffic. Host/private/other-tenant,
IPv6, spoofed-source and unsolicited inbound traffic are denied. Unknown TAPs
and a `closed_taps` gate fail closed. The helper maintains only comment-owned,
exact-TAP/source IPv4 UFW route grants to the existing uplink; it adds no reverse
inbound grant and does not change foreign UFW rules. Restrictive nft policy is
installed before those grants, and an error keeps the affected registered TAPs
closed and unready. Install the updated host network tooling and run its
`--check` as root; a legacy or incomplete policy is not silently delegated.
During install-order upgrades, `--apply` can leave tenants explicitly `FENCED`
while installing the base policy. Only a successful `--check` after helper
reconciliation confirms the complete forwarding path.

Each tenant retains a named `tenant_mtap<N>_egress` counter referenced only by
its public forward rule. Rule chains may be rebuilt atomically for policy
upgrades, but live named counters are never recreated. Registry removal deletes
only that tenant's objects and owned route grant; stop/suspend retains its
registration and cumulative counter. Per-job network policy stays in the guest.
Callers of `set_egress` cannot pass firewall rules, interfaces or paths.

The optional root-managed registry flag `capture_console` defaults to false.
When true, tenant VMM stdout appends to the fixed `console.log` beside that
tenant's disk (root-only, no symlink following). This supports operator image
qualification across base load and suspend/restore; no output path is accepted
from helper RPC callers. Base-kernel-boot console capture is separately owned
by the provisioner.

Trust refusals name the exact offending input or ancestor, numeric uid/gid,
permission mode and symlink status. Root ownership alone does not exempt a
symlink: provision regular files in protected directories rather than relaxing
the no-symlink policy. In particular a system utility reached through an
alternatives/uutils symlink is not a suitable loader input without a regular,
root-owned staged copy.

## Tenant start policy

`Ensure` and `Restore` check `/etc/motor-host/capacity.json`, the sum of all
registered tenants' RAM and VMM headroom against `MemTotal`, the live
`motor-jobs.slice` cap, and the tenant's `resize.json` before attempting a wake.
The helper never writes the jobs-slice drop-in. When the slice is not installed,
the helper requires `/etc/systemd/system/motor-jobs.slice.d/50-capacity.conf` to
match the capacity render exactly. Its `MemoryMax` is the checked sum of
`jobs_min_bytes` and `windows_vm_bytes`, since the Windows VM shares the slice
with direct Linux jobs. `MemoryHigh` is `max - floor(max / 6)`. The
pinned 40 GiB drop-in is `tests/vectors/jobs-slice-capacity.conf`.

A policy refusal replies with `result: "start_refused"`, a typed `case` of
`uplink`, `capacity_json`, `fit`, `live_cap`, `drop_in`, `resize_journal`, `oom`,
`activation` or `post_grow_space`, and a
diagnostic `message`. These refusals do not disable Stop, Metrics, removal or
startup reconciliation. Both snapshot load paths verify the VMM is named
`firecracker` and set and read back `oom_score_adj=-500` before snapshot load.
An OOM protection failure stops and reaps the VMM without consuming saved state,
advancing the generation or attempting a base fallback.

An invalid or missing uplink fences all tenant gates without disabling the
helper socket. Gate-opening requests get the `uplink` case with its diagnostic;
stopped-tenant removal and interrupted-removal replay use the ordinary online
request path without requiring an uplink. Network fencing must succeed before
startup continues; other cleanup and trust barriers are unchanged.

On a dedicated fixture host with a stopped disposable tenant, saved snapshots,
a matching size base, and valid capacity policy, the operator-only
`isolated_tenant_load_paths_verify_firecracker_oom_score` test exercises both
real load paths. Stop motor-host and motor-vm-helper first, set
`MOTOR_VM_TEST_TENANT` to that tenant, and run the helper tests with
`--features linux-service-tests` in a delegated service. This lane requires
root, systemd, KVM and the pinned jailer
and Firecracker; it is not part of the portable suite.

## Staged resize activation

Provisioning publishes `tenants/<tenant>/runner-args.staged` as a root-owned,
mode 0600 regular file. The helper opens it without following symlinks and
requires exactly two JSON fields: `runner_args` (a string, at most 16 KiB decoded)
and `quota_grew` (a boolean). Unknown or missing fields refuse the start.
`tests/vectors/runner-args-staged.json` pins the producer/consumer bytes.
Arguments use the guest's whitespace-separated tenant-runner option grammar,
not shell syntax: host backend, tenant mode, veth networking, matching positive
u32 weight/slot limits, numeric cache/floor values, and both `/Users` and `/home`
workspace and owned-workspace roots. The optional state root is `/tenant/state`.
The helper validates this grammar before sending anything to the guest.

A pending activation forces a cold base load, even if a saved snapshot matches.
The host uses `prepare_start` (tenant id only) to load, PATCH and resume the VM,
then sends `attach_tenant` on the guest control channel. Only after attachment
succeeds does the host call helper `activate` (also tenant id only). The helper
reads its own staged record and sends `apply_resize` on the agent channel with
the staged `runner_args`. Attachment is required because resize drains and
restarts the tenant runner. The guest reply must acknowledge its grow,
argument write and durability barriers. Only after that reply does the helper
fsync its open disk file. When `quota_grew` is true, it then checks host available
bytes against the operating requirement on the disk's filesystem:

```
max(host_floor_bytes + (idle_suspend_s > 0 ? guest_ram_mib * 2^20 : 0))
    + 2 * storage_margin_bytes + growth_allowance_bytes
```

The maximum includes every registered tenant on that filesystem; all arithmetic
is checked. Equality passes. The durable `quota_grew` value, not the current
device and filesystem sizes, decides whether retries need this check. An
activation without quota growth does not probe free space. After these steps the helper unlinks the
staged file and fsyncs its parent directory. A failed directory barrier restores
the staged intent before returning failure.

An activation or post-grow failure returns `start_refused` before wake succeeds,
retains the staged intent, and uses the same `prepare_stop`, stop, clear-running
sequence as Stop. If guest preparation fails, the unclean and running markers
remain for recovery. A failed stop cannot make a pending activation adoptable.
The next start cold-boots and re-delivers the same intent idempotently. Portable
tests cover ordered activation, space boundaries, no-growth, guest and host
barrier refusals, clean-stop recovery and re-delivery. These helper doubles do
not exercise real XFS growth or guest-side barriers; those need separate
validation on a disposable tenant, on a dedicated root/KVM host with a
resize-capable tenant image.

The host calls `activate` on every wake, including adopted VMs and retries; no
pending intent is a no-op success. It verifies the VM is still running before
declaring readiness. Until activation succeeds, the host neither advertises
free tenant capacity nor opens the job relay. The helper additionally fences
job/event relays (52/53) and suspend while intent is pending; control port 54
remains available for attachment and resize. A host crash after attachment or
during activation leaves the staged intent for the next cold start. A helper
restart kills old VMMs and records recovery as usual.

Cold tenants can still advertise capacity to attract their first lease, but
the host withdraws that capacity as soon as a wake starts and restores it only
on success. Startup recovery recognizes a pending-activation adopt refusal and
finishes that tenant's start before the host begins advertising. A lease may
be reserved during startup, but delivery to the runner remains fenced.

The protocol additions fail closed during mixed-version deployment: legacy
`ensure`, `restore`, and `adopt` return `start_refused` without a running success
while activation is pending. Thus an old host cannot adopt or ready the interim
VM. An old helper rejects the new operation names, so a new host cannot skip the
activation handshake either. Deploy both **motor-host** and **motor-vm-helper**;
the existing resize-capable guest image is unchanged (no image build or
`switch-image`). Tests:
`cold_staged_resize_attaches_before_activation_and_never_advertises_or_delivers_early`,
`resize_crash_after_attach_cold_boots_and_redelivers_before_ready`,
`legacy_start_and_adopt_cannot_expose_a_staged_tenant_as_running`, and
`pending_activation_fences_job_relays_and_snapshot_but_not_control`.

### Real staged-resize qualification

`motor-host`'s ignored `isolated_staged_resize_cold_start` test runs the real
`Runtime` retry path, host `Helper`, helper daemon, and guest agent. It requires
root and a dedicated Linux/KVM host with **all registered tenants stopped**,
`motor-host.service` inactive, and the new `motor-vm-helper.service` active.
Use only a disposable tenant named `resize-canary-*`, configured in the selected
host config's bindings. The test never targets `tenant-1`, edits a registry,
mounts a disk, submits real work, or opens the production host journal. It leaves
the throwaway tenant cleanly stopped and does not delete its disk or snapshots.
The helper must admit the peer listed in `/etc/motor-vm-helper.json`; the test
reads protected records as root, temporarily connects as that peer, and restores
root for completion assertions. Run only this test in its isolated nextest
process, not alongside other privileged work.

Prepare the fixture with the ordinary provisioner **offline**: increase its disk
quota so `runner-args.staged` has `quota_grew: true`, and leave it stopped. Do not
bypass the provisioner's host-wide quiescence or pending-intent checks; if another
tenant has pending activation, finish that cutover before preparing this fixture.
Use an existing `resize-v1` image. After deploying the new helper and selecting a
host config that binds the throwaway tenant, run from the repository on the
operator host:

```sh
sudo systemctl stop motor-host.service
sudo systemctl start motor-vm-helper.service
sudo --preserve-env=PATH env \
  MOTOR_VM_TEST_TENANT=resize-canary-1 \
  MOTOR_RESIZE_OPERATOR_ACK=stopped-disposable-tenant \
  MOTOR_HOST_TEST_CONFIG=/etc/motor-host/host.json \
  env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  cargo nextest run --locked -p motor-host --lib \
    -E 'test(=tests::resize_operator::isolated_staged_resize_cold_start)' \
    --run-ignored only --no-tests fail --nocapture
```

The test checks a stopped fixture, staged ownership/mode, resized disk length,
real cold attach/resize success, host readiness, effective weight/cache/floor
arguments via `args_status`, staged-intent removal after the real post-grow check,
and clean-stop marker removal. No skip counts as qualification; retain the
printed PASS and args-status output. A failure leaves intent pending unless all
activation barriers already completed, and cleanup always attempts to stop the
throwaway VM. Investigate any cleanup failure before restarting motor-host.
Portable CI deliberately ignores this test; it does not establish real XFS or
Firecracker behavior.
