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

Optional fields are `idle_suspend_s` (600), `host_floor_bytes` (128 GiB,
minimum: the direct runner's 128 GiB free-space floor),
`memory_headroom_mib` (512, minimum 512), `snapshot_memory_high_mib` (16384,
clamped to guest RAM), `io_weight` (100, range 1–10000), and `max_weight` (the
permit budget, described below). `idle_suspend_s` 0 means the tenant never
suspends. The default memory
limit is `(guest_ram_mib + 512) * 1048576`, with `memory.high` applied **only**
while creating a snapshot. The fixed jail `pids.max` is `vcpu_ceiling + 64`.
Snapshot creation has substantial writeback costs; tune the I/O weight and
headroom on the target host before production use. Weight is proportional
sharing, not an absolute bandwidth guarantee.

`max_weight` is the tenant's budget of concurrent job permits, counted as
summed vCPU claims. It is optional: without it the budget is `vcpu_ceiling`,
which is exactly what the guest runner arguments and host admission used
before, so a record without the key provisions byte for byte as before.
`checked()` never adds the key, so `switch-image` and `repair-from-registry`
compare old records unchanged. When set, it is a positive integer of at most
4294967295 (the guest's `u32` parser), and at least `vcpu_ceiling`; anything else
is refused before any write. Both runner arguments, `--max-weight` and
`--rustc-slots`, carry the budget and stay equal for the guest's own check.

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

## Host capacity and the jobs slice

Before provisioning, install `/etc/motor-host/capacity.json` as a root-owned
regular file, mode 0644 (group/world write permission is refused). It must
contain exactly these six byte-valued, nonnegative u64 integers:

```json
{
  "windows_vm_bytes": 17179869184,
  "host_reserve_bytes": 6442450944,
  "jobs_min_bytes": 25769803776,
  "storage_margin_bytes": 8589934592,
  "growth_allowance_bytes": 68719476736,
  "tenant_memory_reserve_bytes": 2147483648
}
```

`jobs_min_bytes` must be positive. It may be smaller than `windows_vm_bytes`,
but their sum must fit in u64. Missing, unreadable or invalid capacity input
refuses provisioning without writes.
Memory fit uses `/proc/meminfo`'s `MemTotal` in checked bytes, not `MemAvailable`:

```text
sum((guest_ram_mib + memory_headroom_mib) * 1048576 for every tenant)
    + windows_vm_bytes + host_reserve_bytes + jobs_min_bytes <= MemTotal
```

Apply the slice budget before provisioning or installing new host binaries:

```sh
sudo python3 script/tenant-provision/provision.py apply-jobs-slice
```

Under `provision.lock`, this checks fit over every registered tenant and refuses
while any tenant has a `resize.json`. That journal records an unfinished sizing
change; the slice budget must stay fixed until the resize is resumed or aborted.
It refuses before changing anything when
the slice's `MemoryCurrent` is above the proposed `MemoryHigh`, naming the usage.
It writes `/etc/systemd/system/motor-jobs.slice.d/50-capacity.conf`, runs
`systemctl daemon-reload`, and verifies the live `MemoryMax` is no higher than
the budget. Apply or read-back failures are errors; inspect the drop-in and
live limits before retrying. The drop-in is written even if the slice is not
installed yet. Provisioning requires its exact rendered bytes and checks the
live cap too; an unloaded, `not-found` slice requires the same exact file.

`MemoryMax = jobs_min_bytes + windows_vm_bytes` and
`MemoryHigh = MemoryMax - floor(MemoryMax / 6)`. The slice holds the Windows VM
plus direct Linux jobs: `jobs_min_bytes` is what direct Linux jobs keep when
Windows uses its full allowance. The example values render exactly:

```ini
[Slice]
MemoryMax=42949672960
MemoryHigh=35791394134
```

## Tenant allocation

Provisioning checks **bytes** before allocating anything for the tenant, against
the same storage rule motor-host applies on every admission and heartbeat:

```text
host_floor_bytes + (idle_suspend_s > 0 ? guest RAM : 0)   the suspend reserve
  + storage_margin_bytes                                  the storage threshold
  + storage_margin_bytes + growth_allowance_bytes         the operating requirement
  + 2 GiB (mkfs metadata)
  + guest RAM + 512 MiB, only while the size base is absent
```

An operator budget with these values is `/etc/motor-host/capacity.json`. Nothing
is created when free space falls one byte short. The same rule is exposed as
`storage_threshold`, `operating_requirement` and `pre_operation_free_bytes`, and
motor-host keeps the same numbers (`test_storage_requirement_pinned_to_the_cutover_figures`,
`test_pre_operation_requirement_passes_at_exact_and_refuses_one_byte_short`). On a host that also runs `motor-runner serve`, it reads
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
only if absent. An image whose manifest lists the capability `sparse-v1` gets a
disk truncated to the quota, so the guest sees the whole size and only blocks it
writes occupy the host; any other image gets a fully allocated disk as before
and is never reflinked. An existing disk is never rewritten, whichever way it
was created. The base boot asks Firecracker for `discard` on the tenant drive
with the `Sync` IO engine, so the guest's freed blocks reach the host, and it
sets the VMM's `oom_score_adj` to -500 and reads it back, refusing the boot when
either step fails. The disk is formatted XFS with reflink enabled and mounted
only during initial creation of `/tenant/runner-args`. It writes generation zero, a root-only helper registry
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

`provision.py remove-tenant <tenant> [--dry-run | --purge-now]` removes a
departed customer's tenant. Like `switch-image` it refuses while
`motor-host.service` or `motor-vm-helper.service` is active. Under the provision
lock it copies the tenant's records into
`/var/lib/motor-vm/removed/<tenant>-<unix seconds>/records/`, deletes its tap,
drops its binding, provisioning record and registry entry, and moves its home
(disk, suspend and stale snapshots, markers) to `.../home`; the helper deletes
the folder 7 days later, and its startup reconciliation drops the tenant's
firewall rules. `--dry-run` lists everything and changes nothing; `--purge-now`
deletes at once. `removal-layout.json` pins the same list for the helper's
automatic `remove_tenant`. The procedure and how to undo a removal are in
`docs/hosted-host.md`. Tests: `test_remove_tenant_*`,
`test_removal_layout_is_the_one_the_helper_removes`.

## Offline resize

```sh
sudo systemctl stop motor-host.service motor-vm-helper.service
sudo python3 script/tenant-provision/provision.py resize <tenant> \
    [--vcpu-ceiling N] [--guest-ram-mib N] [--cpuset LIST] [--max-weight N] \
    [--idle-suspend-s N] [--disk-quota-bytes N]
sudo python3 script/tenant-provision/provision.py resize --abort <tenant>
```

A resize changes only the named fields; the rest of the record stays as
provisioned. Guest RAM and the disk quota only grow. It is a host-wide outage:
under `provision.lock`, before anything is written, it refuses, each with its own
message, when either service is active; a Firecracker VMM or jailer is running;
any registered tenant has `running.json`, `unclean-stop.json`, a helper cgroup
(`.../motor-vm-helper.service/tenants/tenant-<tap>`) or a jail
(`/var/lib/motor-vm/jail/tenants/<tenant>`); another tenant has a `resize.json`;
any tenant still has a `runner-args.staged` (an earlier resize's activation is
pending: start that tenant once, then stop it); the tenant's image manifest does
not list `resize-v1`; the disk would shrink or the RAM decrease; the disk's length
differs from the recorded quota; the new `vcpu_ceiling` is not 1 or even, or is
above 32; the cpuset overlaps the Windows VM's CPUs (`12-15,28-31`) or another
tenant; free space is below the pre-operation figure of the storage rule above
(the threshold is the largest over every registered tenant, with the new record;
this applies even when only `max_weight` or `idle_suspend_s` changes); the memory
fit fails with the new RAM replacing the old; or the jobs-slice drop-in is absent,
differs from the render, or the live cap check fails. Resize verifies the drop-in
but never writes it.

The journal `tenants/<tenant>/resize.json` (mode 0600) holds the old and new
records, their exact bytes, the old and new `/etc/motor-vm-helper.json` (slot
CPUs follow the cpuset), the runner arguments, whether the new size base existed,
the snapshot set-aside directory, the staged file paths and the phase. Each
write is an fsynced temporary file renamed over it, then a directory fsync.
Every path is recorded before the step that creates it. The steps are: build the
new size base if absent; stage `runner-args.staged` (root-owned, mode 0600, the
helper's `{"runner_args","quota_grew"}` JSON) and copies of the three new files in
`resize-staged/`; move `suspend/` aside to `suspend.resize-<ns>` and create an
empty one, since suspend keys omit the size; then **commit**. After the commit it
replaces the provisioning record, the helper registry and the helper config,
extends `disk.img` sparsely with `truncate`, confirms the staged activation and
removes the journal. The tenant disk is never mounted: on its next cold start the
guest grows XFS and installs the arguments (`crates/motor-vm-helper/README.md`,
"Staged resize activation"). The old snapshots stay in `suspend.resize-<ns>` for
the operator to delete.

After a failure, re-run the same command to resume from the recorded phase;
different options are refused. Before the commit the disk must still have its
old length, after it exactly the old or the new length. `resize --abort` works
only before the commit. It first records phase `aborting` (from then on a forward
resume refuses), moves the snapshots back, removes the staged files, deletes the
size base only if this resize created it and no registered tenant uses it,
verifies the old records, helper config and disk length, and only then removes
the journal. Each step can be repeated, so a failed abort is finished by running
it again. The jobs-slice drop-in stays. After the commit, abort refuses.

While any `resize.json` exists, provisioning, `switch-image`,
`repair-from-registry`, `apply-jobs-slice` and a resize of another tenant refuse
without writing; only resume or abort of that resize and `remove-tenant` run. The
helper refuses every start of the journaled tenant and `install.sh --check` fails.

### Malformed resize journal

A `resize.json` that cannot be read, is not valid JSON, has missing or unknown
keys, another schema version, an unknown phase, or names a base, set-aside or
staged path the resize would not have created is refused by both resize and
abort, and the tenant stays start-refused. Recover by hand with both services
stopped:

1. Compare `/etc/motor-host/provision/<tenant>.json`,
   `/etc/motor-vm-helper/tenants/<tenant>.json` and `/etc/motor-vm-helper.json`
   with the saved rollback copies, and the `disk.img` length with each record's
   `disk_quota_bytes`.
2. If all of them are still the old ones (nothing was committed): move the newest
   `suspend.resize-*` back to `suspend/` if `suspend/` is empty, delete
   `runner-args.staged` and `resize-staged/`, and delete the new size base only if
   no registry entry names its `<vcpu>-<ram>`.
3. If any of them is the new one (the resize committed): write all three files at
   the new size, `truncate -s <new quota> disk.img`, and write
   `runner-args.staged` as the helper expects (root, 0600).
4. Move `resize.json` out of the tenant home (keep it for the incident record),
   then run `install.sh --check` before starting the services.

Tests: `test_resize.py` (`ResizeTests`, loaded by `test_provision.py`).

Portable tests (including the capacity and resize suites): `python3 scripts/run-script-tests.py`.
Real provisioning, networking and suspend/restore qualification require an
operator-controlled host; the portable tests do not claim VM isolation proof.
