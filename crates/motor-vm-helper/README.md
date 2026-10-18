# motor-vm-helper

`motor-vm-helper` is a Linux root service that owns a pool of Firecracker VM
slots and hands the unprivileged runner only file descriptors. No runner
backend uses it in this build: installing it does not change how any runner
instance runs jobs.

[docs/linux-microvm.md](../../docs/linux-microvm.md#motor-vm-helper) describes
its host prerequisites, installation, protocol, restart behavior and tests.
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

The host's `motor_tenant` nftables table is independent of the guest's per-job
`motor_net` table. It drops tenant traffic to the host, other tenants' taps,
private/reserved IPv4 destinations, all IPv6 and unsolicited inbound traffic.
Each drop rule counts blocked packets and bytes. Public egress is masqueraded
per tenant. `set_egress` installs this fixed policy; callers
cannot pass firewall rules or paths. Per-job network policy stays in the guest.
Each tenant has its own input, forward and NAT chains and a named
`tenant_mtap<N>_egress` counter referenced only by its public forward rule.
Reconciliation compares the registry with existing nftables objects, including
at helper startup. One atomic transaction adds new tenant objects or removes
only objects for tenants whose registries were removed. Existing live counters
and other tenants' chains are never flushed or recreated. Incomplete live object
sets are refused rather than reset; stopping or suspending a VM does not disable
its registered tenant or delete its cumulative counter.
