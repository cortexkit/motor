# Tenant provisioning

`python3 script/tenant-provision/provision.py --config /absolute/operator-config.json`
runs once as root on the dedicated Linux VM host. Requires Python 3.9+, XFS
with reflink, `fallocate`, `mkfs.xfs`, `ip`, `mount`, `umount`, cgroup v2 with
CPU/cpuset/memory/pids/I/O controllers, and Firecracker **and jailer 1.17.0** in
`/usr/local/lib/firecracker/1.17.0/`. Deploy `script/tenant-vm-spike/api_schema.py`
and its checksum-pinned release fixtures alongside this directory: every base
boot API request is validated against the published release schema.

The JSON configuration has these required fields:

- `tenant`, `vm`: single path components; `sub`: the lease broker's account
  (`acct:<ULID>`) or organization (`org:<ULID>`) identity;
- `image_version`: an existing immutable directory in `/srv/build/vm/tenant-images`;
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
new size base is needed. It builds `base/<ceiling>-<ram>/{base.vmstate,base.memory}`
only if absent. It fully allocates a new disk (never reflinks it), formats XFS
with reflink enabled, and mounts it only during initial creation of
`/tenant/runner-args`. It writes generation zero, a root-only helper registry
entry under `/etc/motor-vm-helper/tenants`, and the host-readable
`/etc/motor-host/tenants.json` binding map. Existing tenants, tap indices and VM
bindings are refused. Failure intentionally retains partially created disk
data; inspect it manually, never delete a tenant disk as automatic rollback.

Portable tests: `python3 -W error script/tenant-provision/test_provision.py`.
Real provisioning, networking and suspend/restore qualification require an
operator-controlled host; the portable tests do not claim VM isolation proof.
