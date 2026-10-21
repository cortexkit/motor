# Tenant provisioning

`python3 script/tenant-provision/provision.py --config /absolute/operator-config.json`
runs once as root on the dedicated Linux VM host. Install and activate the
[VM storage bind mount](../../docs/linux-microvm.md#install) first; all root-run
paths use `/var/lib/motor-vm`, not the `/srv/build/vm` source spelling. Requires Python 3.9+, XFS
with reflink, `fallocate`, `mkfs.xfs`, `ip`, `mount`, `umount`, cgroup v2 with
CPU/cpuset/memory/pids/I/O controllers, and Firecracker **and jailer 1.17.0** in
`/usr/local/lib/firecracker/1.17.0/`. Deploy `script/tenant-vm-spike/api_schema.py`
and its checksum-pinned release fixtures alongside this directory: every base
boot API request is validated against the published release schema.

The JSON configuration has these required fields:

- `tenant`, `vm`: single path components; `sub`: the lease broker's account
  (`acct:<ULID>`) or organization (`org:<ULID>`) identity;
- `image_version`: an existing immutable directory in `/var/lib/motor-vm/tenant-images`;
- `vcpu_ceiling`, `guest_ram_mib`, `disk_quota_bytes`, `cpuset`, `tap_index` (1–254);
- `resolvers`: public IPv4 resolver addresses, also configured in the host agent.

Optional fields are `idle_suspend_s` (600), `host_floor_bytes` (128 GiB),
`memory_headroom_mib` (512, minimum 512), `snapshot_memory_high_mib` (16384,
clamped to guest RAM), and `io_weight` (100, range 1–10000). The default memory
limit is `(guest_ram_mib + 512) * 1048576`, with `memory.high` applied **only**
while creating a snapshot. The fixed jail `pids.max` is `vcpu_ceiling + 64`.
Snapshot creation has substantial writeback costs; tune the I/O weight and
headroom on the target host before production use. Weight is proportional
sharing, not an absolute bandwidth guarantee.

Choose a cpuset excluding the host runner's reserved Windows CPUs. Each tap
index must be unique and reserves a dedicated no-login uid `400000 + index`;
the helper refuses overlaps with job accounts, slot accounts and allowed peers.
The provisioning service needs cgroup delegation; it uses a separate
`motor-tenant-provision` cgroup parent. The provisioner kernel-boots a size base in a network
namespace with no uplink. The image must boot read-only with its tmpfs root
overlay and host-only tenant agent enabled, but the runner disabled until
`attach_tenant`. Its version manifest contains `version`, `kernel_sha256`,
`rootfs_sha256` and the image toolchain `versions`. Image toolchain pins belong
to the tenant-image builder, not this provisioning tool.

Provisioning checks **bytes** before allocating anything for the tenant: disk
quota + suspend RAM + host floor, plus RAM and the temporary placeholder when a
new size base is needed. On a host that also runs `motor-runner serve`, it reads
each runner's live arguments (`--free-space-floor-bytes` and `--state-root`,
defaulting as the runner does). For every runner whose state shares the
filesystem of `/var/lib/motor-vm`, it refuses unless the free space left after
the disk, the suspend snapshot and any new size base stays at or above that
runner's refusal floor, below which the runner refuses jobs. The refusal reads
"would leave X GiB free ... below motor-runner pid N's refusal floor Y GiB".
The runner's higher eviction threshold is not reserved: evicting idle
workspaces there is the runner's normal way to free space. Provisioning also refuses if a
runner's arguments cannot be parsed, or if its state root comes from an
environment it cannot read
(`test_runner_on_the_same_filesystem_keeps_its_refusal_floor_before_any_mutation`).
It builds `base/<ceiling>-<ram>/{base.vmstate,base.memory}`
only if absent. It fully allocates a new disk (never reflinks it), formats XFS
with reflink enabled, and mounts it only during initial creation of
`/tenant/runner-args`. It writes generation zero, a root-only helper registry
entry under `/etc/motor-vm-helper/tenants`, and the host-readable
`/etc/motor-host/tenants.json` binding map. Existing tenants, tap indices and VM
bindings are refused. Failure intentionally retains partially created disk
data; inspect it manually, never delete a tenant disk as automatic rollback.

`provision.py repair-from-registry <tenant>` fixes one known drift: an image
swapped by hand, before `switch-image` existed, left the provisioning record
(`/etc/motor-host/provision/<tenant>.json`) naming the old image while the
helper registry names the image that actually boots, and `switch-image` refuses
such records. The repair copies the registry's `image_version` into the
provisioning record only when that image exists, its files are root-owned, it
passes the image inventory check and its manifest names it, and every other
registry field is exactly what provisioning would have written from the
record. It replaces the record atomically, keeping its mode and key order, so
`install.sh --check` accepts it, and prints the one change it made. Records
that already agree are left alone. Tests:
`test_switch_image_refuses_the_live_mismatch_until_it_is_repaired`,
`test_repair_refuses_without_changing_anything_unless_only_the_image_differs`.

`provision.py switch-image <tenant> <image>` refuses while `motor-host.service`
or `motor-vm-helper.service` is active, and the refusal names the clean stop:
`sudo systemctl stop motor-host.service motor-vm-helper.service`. Stopping the
helper stops each running tenant VM cleanly (guest drain, sync and freeze, then
the VM) before it exits, so the switch never cuts a guest's power. If the
helper's `running.json` marker is still in the tenant home, the last stop was
not clean; the switch goes ahead and warns that the next start runs the
in-guest recovery check. The procedure is in `docs/hosted-host.md`. Tests:
`test_switch_image_refuses_when_either_service_is_running`,
`test_switch_image_warns_when_the_last_vm_stop_was_unclean`.

Portable tests: `python3 -W error script/tenant-provision/test_provision.py`.
Real provisioning, networking and suspend/restore qualification require an
operator-controlled host; the portable tests do not claim VM isolation proof.
