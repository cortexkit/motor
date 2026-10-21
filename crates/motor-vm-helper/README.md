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
