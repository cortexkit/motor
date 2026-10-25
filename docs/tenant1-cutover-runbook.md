# Tenant 1 stage-2 cutover runbook

This runbook moves the fleet's Linux jobs from the build server's direct runner
onto the hosted path: tenant 1 on host `ovh-1`, which is the build server
itself. It is the operator procedure for the code that adds permit budgets
(`max_weight`), sparse tenant disks with storage admission, offline resize,
`/etc/motor-host/capacity.json` with the jobs-slice drop-in, fleet routing and
the direct runner's `--linux-cpus` option. Run it only after that code has
merged and been built; the resize is an operator step, never part of a merge.

The whole procedure is a host-wide maintenance outage for hosted work. The
Windows instance (`motor-runner@windows.service`) keeps serving throughout.

Everything on the server runs as `ubuntu` with `sudo` from the checkout at
`/srv/build/src/motor`, as in [hosted-host.md](hosted-host.md). Commands for
ck-motor run on the operator machine that hosts the SubC module.

Related documents: [hosted-host.md](hosted-host.md) (install, start, stop),
[hosted-cutover.md](hosted-cutover.md) (the general hosted-route rules),
[drain-and-rollback.md](drain-and-rollback.md) (runner drain and deploy),
[scheduling.md](scheduling.md) (direct-runner capacity) and
[`script/tenant-provision/README.md`](../script/tenant-provision/README.md)
(capacity, storage rule, resize and its journal).

## Owner values

These are the owner's decisions. Use them exactly; do not round or derive
others.

### Host facts (measured on ovh-1)

| Fact | Value |
| --- | --- |
| `MemTotal` | 131,491,984 kB (134,647,791,616 bytes, 125.4 GiB) |
| Threads | 32 (thread *n* and *n*+16 share a core) |
| Windows VM CPUs | `12-15,28-31` |
| Free space on `/srv/build` (holds `/var/lib/motor-vm`) before the cutover | 330 GB (330 × 10^9 bytes) |

### `/etc/motor-host/capacity.json`

Root-owned regular file, mode 0644, exactly these six keys, all bytes:

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

That is 16 GiB for the Windows job VM, 6 GiB host reserve, 24 GiB for direct
Linux jobs, an 8 GiB storage margin, a 64 GiB growth allowance and a 2 GiB
tenant memory reserve.

Memory fit: (79,872 + 512) MiB for tenant 1 + 16 + 6 + 24 GiB =
133,680,857,088 bytes (124.5 GiB) ≤ `MemTotal`.

### Jobs slice

`provision.py apply-jobs-slice` writes
`/etc/systemd/system/motor-jobs.slice.d/50-capacity.conf`. From the values
above it renders exactly:

```ini
[Slice]
MemoryMax=42949672960
MemoryHigh=35791394134
```

`MemoryMax` is `jobs_min_bytes + windows_vm_bytes` (40 GiB): the slice holds the
Windows VM and the direct Linux jobs, and Linux jobs keep 24 GiB while Windows
uses its full 16 GiB. `MemoryHigh` is `MemoryMax − ⌊MemoryMax / 6⌋`. This is the
corrected owner ruling that the provisioner implements; it replaces the
earlier 20 GiB / 24 GiB slice figures. A direct job's unit `MemoryMax` is
clamped to the slice `MemoryMax`, and a job is refused before start only if the
16 GiB floor every job asks for cannot fit the slice.

### Tenant 1

| Field | Before (rollback size) | After |
| --- | --- | --- |
| `vcpu_ceiling` | 8 | 24 |
| `cpuset` | `8-11,24-27` | `0-11,16-27` |
| `guest_ram_mib` | 28672 (28 GiB) | 79872 (78 GiB) |
| `disk_quota_bytes` | 107374182400 (100 GiB) | 644245094400 (600 GiB, sparse growth) |
| `max_weight` | absent (budget = `vcpu_ceiling`) | 64 |
| `idle_suspend_s` | 600 | 0 (never suspends) |
| `image_version` | the saved current image | the new image (lists `resize-v1` and `sparse-v1`) |
| `host_floor_bytes`, `tap_index`, `memory_headroom_mib`, `snapshot_memory_high_mib`, `io_weight` | unchanged | unchanged |

Confirm the "before" column against the saved record in forward step 1; the
saved record wins if it differs, and then stop and ask the owner.

### Placeable size and CLOUD ceilings

- The 78 GiB guest is 79,872 MiB. Less the 2,048 MiB reserve it advertises at
  most 77,824 MiB as `free_mem_mib`.
- The broker needs `max(16384, 3584 × N)` MiB. The largest N with
  `3584 × N ≤ 77,824` is **21** (75,264 MiB); 22 needs 78,848.
- CLOUD's ceilings for ovh-1: **`max_job_vcpus` 21**, **`max_job_mem_mib`
  75,264**.
- Jobs of 22–24 vCPUs get the broker's 422 `no_host_fits`, are corrected to
  the direct runner before Run, exceed its 16 and are refused before start with
  `runner_full` (detail ends `; hosted max_vcpus 21`).
- Forward canary: 21 vCPUs. Rollback canary on the old 28 GiB guest
  (28,672 − 2,048 = 26,624 MiB advertised): 7 vCPUs (25,088 MiB); 8 vCPUs would
  need 28,672 and does not place.

The template test `test_placeable_n_comes_from_guest_ram_and_the_reserve`
(`script/hosted-host/test_hosted_host.py`) computes N = 21 for 79,872 MiB and
N = 7 for 28,672 MiB.

### Direct runner

`motor-runner.service` (instance `main`) changes from `--max-weight 80` to
`--linux-cpus 12-15,28-31 --max-weight 16`: Linux jobs run only on the 8
threads outside tenant 1's cpuset, shared with the Windows VM as today. A claim
up to 16 is admitted (placed on at most 8 CPUs); a claim above 16 is refused
before start with `runner_full`, never queued. Jobs over the broker's 7,200 s
lease limit with at most 16 vCPUs run here.

### Free-space figures

All in bytes on the filesystem of `/var/lib/motor-vm`; "free" is
`f_bavail × f_frsize`, the figure `provision.py` and motor-host both use.

| Figure | Value |
| --- | --- |
| Storage threshold, tenant 1 after (128 GiB floor + 0 suspend reserve at idle 0 + 8 GiB margin) | 146,028,888,064 (136 GiB) |
| Latch release point (threshold + 8 GiB) | 154,618,822,656 (144 GiB) |
| Operating requirement (release + 64 GiB growth allowance) | 223,338,299,392 (208 GiB) |
| Resize pre-operation requirement (operating + 2 GiB metadata + 78.5 GiB for the absent `24-79872` base) | 309,774,516,224 (288.5 GiB) |
| Free space after `switch-image` builds the new image's `8-28672` base, from 330 × 10^9 | about 299,935,228,928 or fewer |
| Shortfall to reclaim before the resize | about 9,839,287,296 (9.2 GiB) or more |
| Storage threshold, tenant 1 before (128 GiB floor + 28 GiB suspend reserve + 8 GiB) | 176,093,659,136 (164 GiB) |
| Old-size reprovision requirement, old base present (164 + 8 + 64 + 2 GiB) | 255,550,554,112 (238 GiB) |
| Old-size reprovision requirement, old base absent (+ 28.5 GiB) | 286,152,196,096 (266.5 GiB) |

The new `24-79872` base takes about 78.5 GiB, so a resize started at
309,774,516,224 free leaves about 225,485,783,040 (210 GiB), above the
208 GiB the helper checks after the guest grows its disk.

## Operator-filled placeholders

Fill these in before step 1, each with its verification. A placeholder that
cannot be verified stops the cutover.

| Placeholder | What it is | Verification |
| --- | --- | --- |
| `<CK_MOTOR_CONFIG>` | Absolute path of the ck-motor config file the running module loads (from `--config` or `CK_MOTOR_CONFIG`; motor does not know it) | Show that the running module was started with it: `ps -o args= -p <ck-motor pid>` names `--config <CK_MOTOR_CONFIG>`, or its environment has `CK_MOTOR_CONFIG=<CK_MOTOR_CONFIG>`. Then `shasum -a 256 <CK_MOTOR_CONFIG>` matches the copy saved in step 1. |
| `<SUBC_RESTART>` | The SubC command that restarts the ck-motor module so it rereads its config (SubC owns the lifecycle; there is no `ck module restart`) | Run it once before the cutover with the config unchanged; the ck-motor pid changes, the module's startup log appears after the command's time, and a small `ck motor run --timeout 60 -- true` exits 0. |
| `<CLOUD_SET_CEILINGS>` | CLOUD's own operator command that sets ovh-1's `max_job_vcpus` and `max_job_mem_mib` | Only CLOUD runs it. Its read-back (next row) must show the new values. |
| `<CLOUD_READ_CEILINGS>` | CLOUD's own command that reads ovh-1's ceilings back | Read them before the cutover; the output must name ovh-1 and both fields. Save it as the rollback ceilings in step 1. |
| `<OLD_IMAGE>` | Tenant 1's current `image_version` | Equals `image_version` in both `/etc/motor-host/provision/tenant-1.json` and `/etc/motor-vm-helper/tenants/tenant-1.json`. |
| `<NEW_IMAGE>` | The new tenant image, in `/var/lib/motor-vm/tenant-images/<NEW_IMAGE>/` | `sudo python3 -c 'import json,sys; m=json.load(open(sys.argv[1])); assert {"resize-v1","sparse-v1"} <= set(m.get("capabilities",[])), m.get("capabilities"); print(m["version"])' /var/lib/motor-vm/tenant-images/<NEW_IMAGE>/manifest.json` prints `<NEW_IMAGE>`. |
| `<NEW_REV>` | The merged motor revision that contains this code | `git -C /srv/build/src/motor rev-parse HEAD` equals it after `script/runner/sync.sh`. |
| `<OLD_REV>` | The motor revision the host binaries were built from before the cutover | Recorded in step 1; a checkout of it builds the saved binaries' versions. |
| `<NETWORK_ROUTING>` | The fleet's current `network_routing` value, `true` or `false` | Read from the saved config in step 1 (absent means `false`). The cutover never changes it. |
| `<CANARY_REPO>` | A throwaway Git checkout on the operator machine, under `/Users` or `/home`, used only for canaries | `git -C <CANARY_REPO> rev-parse --show-toplevel` prints `<CANARY_REPO>` (the canonical path the config entry must name). |

Create the canary checkout once:

```sh
mkdir -p <CANARY_REPO> && cd <CANARY_REPO> && git init -q && echo canary > README && git add README && git commit -qm canary
```

## ck-motor config fragments

Each fragment is merged into the existing config at `<CK_MOTOR_CONFIG>`,
keeping every other key (credentials, `broker_url`, runner targets). The config
refuses unknown keys, so use exactly these names.

Forward (fleet hosted, Boat-eligible):

```json
{
  "hosted_runners": true,
  "hosted_repositories": [],
  "spill_to_hosted": false,
  "hosted_fleet_routing": true,
  "network_routing": <NETWORK_ROUTING>
}
```

Rollback (direct only):

```json
{
  "hosted_runners": false,
  "hosted_repositories": [],
  "spill_to_hosted": false,
  "shadow_to_hosted": false,
  "hosted_fleet_routing": false
}
```

`hosted_fleet_routing: false` alone is not a rollback: with `hosted_runners`
still true the global route keeps sending Linux jobs hosted. With the rollback
fragment, a Linux job with a 600 s timeout and a job with no timeout both route
direct with no lease request
(`rollback_fragment_routes_bounded_and_unbounded_jobs_direct_without_leases`,
`crates/motor-module/src/motor/fleet_routing_tests.rs`).

Under the forward fragment, Linux and platform-omitted jobs with a 10–7,200 s
timeout and 1–64 vCPUs go hosted with `allow_spill: true`; Windows and macOS
jobs, jobs outside those bounds and gate jobs (direct `ck motor run`) stay on
the build server. A `hosted_repositories` entry takes precedence over the fleet
route and sends `allow_spill: false`; the canaries use that.

Apply any config change the same way:

```sh
cp <CK_MOTOR_CONFIG> <CK_MOTOR_CONFIG>.bak-$(date -u +%Y%m%dT%H%M%SZ)
# edit <CK_MOTOR_CONFIG>, then:
<SUBC_RESTART>
```

and verify the restart as in the placeholder table.

## Prerequisites

1. `<NEW_REV>` is merged and synced to the server (`script/runner/sync.sh` from
   the operator machine). The portable gates passed for it on the build server:
   `python3 scripts/run-script-tests.py`, `cargo fmt --all --check` and
   `cargo test --workspace`. Record any known baseline failure by name.
2. The broker-double tests that this procedure leans on passed at `<NEW_REV>`:
   - `twenty_one_vcpu_canary_qualifies_against_the_78_gib_guest_advertisement`
     (`crates/motor-module/src/motor/fleet_capacity_tests.rs`): a 21-vCPU claim
     qualifies against the 78 GiB guest's 77,824 MiB and 22 does not;
   - `fleet_burst_over_one_heartbeat_waits_through_host_full_and_no_capacity`
     and `fleet_capacity_wait_takes_host_full_and_no_capacity_in_either_order`
     (same file): a full tenant's surplus waits within the queue budget and is
     not counted lost or failed;
   - `oversized_fleet_jobs_are_refused_before_start_naming_both_routes_limits`
     (same file): 22-vCPU and 17-vCPU long jobs are refused before start;
   - `rollback_fragment_routes_bounded_and_unbounded_jobs_direct_without_leases`
     (`fleet_routing_tests.rs`);
   - `helper_start_refusal_is_host_full_before_run_and_holds_the_tenant_until_a_start_succeeds`
     (`crates/motor-host/tests/it/transport.rs`) and
     `storage_latch_refuses_one_byte_below_threshold_and_releases_only_after_the_margin`
     (`crates/motor-host/tests/it/capacity.rs`).
3. Boat overflow: fleet leases carry `allow_spill: true`
   (`fleet_linux_default_platform_jobs_allow_hosted_spill_but_gates_stay_direct`),
   and CLOUD has confirmed with its own canary that a spilled Linux lease is
   placed on Boat for this account. Record CLOUD's confirmation. Without it, a
   full tenant only waits within the queue budget.
4. `<NEW_IMAGE>` is installed and verified (placeholder table).
5. Time for the [Rehearsals](#rehearsals) on the throwaway tenant. They need
   `capacity.json` and the jobs-slice drop-in, so they run inside the outage
   right after step 4.2, and their recorded results gate step 4.3.
6. `admit_direct_exec` is true in `<CK_MOTOR_CONFIG>` for the canaries (`ck
   motor run` needs it). If it is not, add it with the canary entry in step 7
   and remove it with that entry.
7. A second person is on call to answer escalations, and the owner is
   reachable.

## Tools

Save these once on the server. They are used throughout.

```sh
sudo install -d -m 0700 /root/tenant1-cutover /root/tenant1-cutover/saved
sudo tee /root/tenant1-cutover/admin.py >/dev/null <<'PY'
#!/usr/bin/env python3
"""motor-host admin request for tenant-1: drain, idle (status plus an idle
check), or memory (the free_mem_mib motor-host advertises for this running
tenant: floor_MiB(guest mem_available_bytes) - 2048, saturating at 0)."""
import json, socket, struct, sys

op = sys.argv[1]
request = {"tenant": "tenant-1", "op": "status" if op in ("idle", "memory") else op}
if op == "drain":
    request.update(deadline_s=10, exit_after=False)
data = json.dumps(request).encode()
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.settimeout(60)
    s.connect("/run/motor-host/admin.sock")
    s.sendall(struct.pack(">I", len(data)) + data)
    with s.makefile("rb") as f:
        size, = struct.unpack(">I", f.read(4))
        payload = f.read(size)
if len(payload) != size:
    sys.exit("short admin reply")
reply = json.loads(payload)
if reply.get("ok") is not True:
    sys.exit(f"admin request refused: {reply}")
if op == "idle":
    print("queue_depth", reply.get("queue_depth"), "running_jobs", reply.get("running_jobs"))
    if reply.get("queue_depth") != 0 or reply.get("running_jobs") != []:
        sys.exit("NOT IDLE")
elif op == "memory":
    available = (reply.get("health") or {}).get("mem_available_bytes")
    if available is None:
        sys.exit("no mem_available_bytes in the guest health: motor-host advertises 0")
    print("mem_available_bytes", available, "advertised free_mem_mib", max(0, available // 1048576 - 2048))
else:
    print(json.dumps(reply, indent=2))
PY
sudo tee /root/tenant1-cutover/stop-tenant.py >/dev/null <<'PY'
#!/usr/bin/env python3
"""Clean-stop one tenant VM through the helper, as the motor-host user."""
import json, socket, sys

tenant = sys.argv[1]
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
    s.settimeout(60)
    s.connect("/run/motor-vm-helper.sock")
    s.sendall(json.dumps({"op": "stop_tenant", "tenant": tenant}).encode() + b"\n")
    with s.makefile("rb") as f:
        reply = json.loads(f.readline())
print(reply)
if reply.get("result") != "tenant" or reply.get("running") is not False or reply.get("recovery_required", False):
    sys.exit("stop not confirmed clean; do not continue")
PY
sudo tee /root/tenant1-cutover/free.sh >/dev/null <<'SH'
#!/bin/sh
# Free bytes as provision.py and motor-host count them (f_bavail * f_frsize).
python3 -c 'import shutil; print(shutil.disk_usage("/var/lib/motor-vm").free)'
SH
sudo chmod 0755 /root/tenant1-cutover/*.py /root/tenant1-cutover/free.sh
```

`stop-tenant.py` must run as the `motor-host` user, the only uid the helper
admits, and that user cannot read `/root`. Whenever it is used, copy it out
first: `sudo install -m 0644 /root/tenant1-cutover/stop-tenant.py /run/stop-tenant.py`,
then `sudo runuser -u motor-host -- python3 /run/stop-tenant.py <tenant>`.

## Forward procedure

### 1. Save the rollback state

On the server:

```sh
cd /srv/build/src/motor
S=/root/tenant1-cutover/saved
sudo install -m 0600 /etc/motor-host/provision/tenant-1.json $S/provision.json
sudo install -m 0600 /etc/motor-vm-helper/tenants/tenant-1.json $S/registry.json
sudo install -m 0600 /etc/motor-vm-helper.json $S/helper.json
sudo install -m 0600 /etc/motor-host/tenants.json $S/tenants.json
sudo install -m 0700 /usr/local/bin/motor-host $S/motor-host
sudo install -m 0700 /usr/local/libexec/motor-vm-helper $S/motor-vm-helper
sudo install -m 0700 /usr/local/libexec/motor-vm-loader $S/motor-vm-loader
cp -p ~/.config/systemd/user/motor-runner.service ~/motor-runner.service.pre-cutover
sudo python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["image_version"])' $S/provision.json | sudo tee $S/image_version
sudo sh -c 'sha256sum /root/tenant1-cutover/saved/* > /root/tenant1-cutover/saved.sha256'
```

Record `<OLD_REV>` (the revision the saved binaries came from) in
`$S/old_rev`. On the operator machine save `<CK_MOTOR_CONFIG>` as
`<CK_MOTOR_CONFIG>.pre-cutover` and note `<NETWORK_ROUTING>` from it. Have CLOUD
run `<CLOUD_READ_CEILINGS>` for ovh-1 and save the output as the rollback
ceilings (also on the server as `$S/cloud_ceilings.txt`).

Check: the saved `image_version` is `<OLD_IMAGE>`; the saved record matches the
"before" column of the tenant table; neither saved record has a `max_weight`
key (the [rollback preflight](#rollback-preflight) passes on them).

### 2. Stop fleet Linux intake and drain

Drain tenant 1 through motor-host, then confirm it is idle (repeat until it
passes):

```sh
sudo /root/tenant1-cutover/admin.py drain
sudo /root/tenant1-cutover/admin.py idle
```

Drain the direct runner's Linux instance, as `ubuntu`, holding admission closed
so its restart policy cannot admit jobs:

```sh
~/.cargo/bin/motor-runner drain --instance main --drain-timeout-s 1800 --exit-after-drain false
```

New and queued Linux jobs are then refused before start with `runner_draining`
(`ran: none`); callers retry. Started Linux jobs keep running in their units.
Wait until only the Windows VM's unit is left in the jobs slice:

```sh
systemd-cgls --no-pager -u motor-jobs.slice
```

The Windows instance is not drained and keeps serving.

### 3. Stop the hosted services in order

motor-host first, so no lease can wake the tenant; ovh-1 becomes unselectable
60 s after its last heartbeat. Then confirm the clean stop of every tenant,
then stop the helper. Never stop the helper while a tenant VM runs.

```sh
sudo systemctl stop motor-host.service
sudo install -m 0644 /root/tenant1-cutover/stop-tenant.py /run/stop-tenant.py
sudo runuser -u motor-host -- python3 /run/stop-tenant.py tenant-1
# Repeat for every other registered tenant (sudo ls /etc/motor-vm-helper/tenants/).
pgrep -af 'firecracker|jailer' && echo 'A VMM is still running: stop here'
sudo systemctl stop motor-vm-helper.service
```

### 4. Capacity, slice, binaries, image, space, resize

This order is fixed: `capacity.json` and the slice before the new binaries
(which refuse every start without them), then `switch-image`, then the
free-space measurement and reclamation, then the resize.

4.1 Write `capacity.json` with the exact owner values and check its owner and
mode:

```sh
sudo install -d -o root -g root -m 0755 /etc/motor-host
sudo tee /etc/motor-host/capacity.json >/dev/null <<'JSON'
{
  "windows_vm_bytes": 17179869184,
  "host_reserve_bytes": 6442450944,
  "jobs_min_bytes": 25769803776,
  "storage_margin_bytes": 8589934592,
  "growth_allowance_bytes": 68719476736,
  "tenant_memory_reserve_bytes": 2147483648
}
JSON
sudo chown root:root /etc/motor-host/capacity.json
sudo chmod 0644 /etc/motor-host/capacity.json
stat -c '%U:%G %a' /etc/motor-host/capacity.json   # root:root 644
```

4.2 Apply the jobs slice (this uses `<NEW_REV>`'s `provision.py`) and read it
back:

```sh
cd /srv/build/src/motor
sudo python3 script/tenant-provision/provision.py apply-jobs-slice
cat /etc/systemd/system/motor-jobs.slice.d/50-capacity.conf
systemctl show -p MemoryMax,MemoryHigh motor-jobs.slice
```

Expect `MemoryMax=42949672960` and `MemoryHigh=35791394134` in both. A refusal
naming `MemoryCurrent` means jobs still occupy the slice: finish step 2.

Now run the [Rehearsals](#rehearsals) and record their results.

4.3 Install the new host binaries. With an installed record the installer
keeps the installed sizing and image; it only builds and installs:

```sh
cd /srv/build/src/motor
git rev-parse HEAD   # <NEW_REV>
sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --build \
  --host-id <host-id> --tenant-sub <tenant-sub>
```

Answer `y` only to binary and unit changes; the tenant record, registry and
helper config must show no change. Do not start anything.

4.4 Switch tenant 1 to the new image. This builds the new image's `8-28672`
size base:

```sh
sudo python3 script/tenant-provision/provision.py switch-image tenant-1 <NEW_IMAGE>
```

Keep its printed rollback line. If it moved a stale suspend snapshot, note the
printed path.

4.5 Measure free bytes and compare with the resize requirement,
309,774,516,224:

```sh
sudo /root/tenant1-cutover/free.sh
```

If the figure is below 309,774,516,224 (expected: about 299,935,228,928),
reclaim space, re-measuring after each step, and stop as soon as it passes:

1. Remove the old image's size base, which no tenant references after the
   switch (about 30 GB):

   ```sh
   B=/var/lib/motor-vm/tenant-images/<OLD_IMAGE>/base/8-28672
   if sudo sh -c 'grep -l "\"image_version\": \"<OLD_IMAGE>\"" /etc/motor-vm-helper/tenants/*.json'; then
     echo 'a registered tenant still uses <OLD_IMAGE>: do not remove its base'
   else
     sudo rm -r -- "$B"
   fi
   sudo /root/tenant1-cutover/free.sh
   ```

   A rollback to `<OLD_IMAGE>` rebuilds this base; the rollback space figures
   already count that.
2. Delete the stale suspend snapshot `switch-image` moved aside in 4.4, if
   any (`/var/lib/motor-vm/tenants/tenant-1/suspend.stale-<OLD_IMAGE>-<ns>`).
   It belongs to the old image and no start can select it.
3. If still short, the remaining source is the drained direct runner's idle
   workspaces. No command evicts them on demand, and deleting them by hand
   bypasses the runner's catalog, so stop here and escalate to the owner with
   the measured figure.

4.6 Resize tenant 1:

```sh
sudo python3 script/tenant-provision/provision.py resize tenant-1 \
  --vcpu-ceiling 24 --cpuset 0-11,16-27 --guest-ram-mib 79872 \
  --disk-quota-bytes 644245094400 --max-weight 64 --idle-suspend-s 0
```

It refuses before any change on a precondition (its message names which). On
`resize needs N free bytes ... ; M are available`, go back to 4.5. If it fails
or the host crashes after it has started, re-run the same command to resume, or
follow [Before the resize commit](#before-the-resize-commit). It ends with
`tenant-1: resized; the old suspend snapshots are kept in .../suspend.resize-<ns>`.
Delete that set-aside directory only after the cutover is accepted.

### 5. Check the installation

```sh
sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --check
```

It verifies the installed record and helper registry agree on every field,
the helper config equals the render from the record, the cpuset avoids the
Windows CPUs, `capacity.json`, the memory fit, the jobs-slice live cap, and
that no `resize.json` exists. It must pass with no problems.

### 6. CLOUD ceilings

Send CLOUD the values. CLOUD runs, for ovh-1:

```text
<CLOUD_SET_CEILINGS>   # max_job_vcpus 21, max_job_mem_mib 75264
<CLOUD_READ_CEILINGS>
```

The read-back must show `max_job_vcpus` 21 and `max_job_mem_mib` 75264 for
ovh-1. Record it. Do not start motor-host before this read-back.

### 7. Start and run the canaries

Start the hosted services:

```sh
sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --start
sudo journalctl -fu motor-host
```

The tenant is stopped until the first lease wakes it, so motor-host advertises
`free_mem_mib` 79,872 − 2,048 = **77,824** and `free_vcpus` 64. motor-host does
not log its heartbeat; CLOUD can read the last heartbeat back if a figure is
in doubt.

On the operator machine, add the temporary canary entry (with
`admit_direct_exec: true` if needed) to `<CK_MOTOR_CONFIG>` and restart:

```json
{ "hosted_repositories": ["<CANARY_REPO>"] }
```

```sh
<SUBC_RESTART>
```

**Cold canary** (the first start cold-boots the resized tenant and delivers its
staged activation: the guest grows its filesystem and installs the new runner
arguments):

```sh
T0=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ck motor run --cwd <CANARY_REPO> --vcpus 21 --timeout 600 -- 'head -1 /proc/meminfo; nproc'
```

On the server:

```sh
sudo journalctl -u motor-host.service -u motor-vm-helper.service --since "$T0" --no-pager \
  | grep -E 'tenant_wake_started|tenant_wake_ready|lease_connected|tenant_start_refused|host_storage_low'
```

It passes only if all of these hold:

- the command exits 0 and prints the guest's `MemTotal`, below 81,788,928 kB
  (79,872 MiB), not the build server's 131,491,984 kB;
- ovh-1 logs `tenant_wake_ready` and `lease_connected` for the canary in the
  window, and no `tenant_start_refused` or `host_storage_low`;
- the job was not routed direct: no refusal, and nothing ran on the build
  server. Its stored ck-motor job record has no `routed_direct_reason`.

Failure outcomes, each a stop before intake:

- `no_host_fits` (a refusal whose detail names `hosted max_vcpus`): CLOUD's
  ceilings are not 21 / 75,264. Re-read them (step 6).
- `no_capacity` or a `hosted_host_full` wait that never admits: motor-host is
  not advertising. Look for `host_storage_low` ([latched stall](#latched-stall-recovery))
  or `tenant_start_refused` (below).
- `tenant_start_refused` with `case` = `capacity_json`, `fit`, `live_cap`,
  `drop_in`, `resize_journal`, `activation` or `post_grow_space`: fix the named
  input (step 4.1, 4.2, 4.6 or the space in 4.5); the tenant retries a start at
  most once per 60 s.
- The job ran anywhere other than ovh-1's tenant (another host's output or the
  build server's `MemTotal`): stop and escalate to the owner.

**Warm canary**: after the cold canary completes, the tenant stays running
(idle 0 never suspends). Record its advertised figure:

```sh
sudo /root/tenant1-cutover/admin.py memory
```

Record `advertised free_mem_mib`. It must be at least **75,264**; then run the
same 21-vCPU canary again with the same pass checks. If the figure is below
75,264 or the warm canary fails, stop and escalate to the owner before any
intake; do not raise CLOUD's ceilings or lower the reserve to make it pass.

Remove the canary entry (and `admit_direct_exec` if it was added for this) from
`<CK_MOTOR_CONFIG>` and run `<SUBC_RESTART>` before step 8.

### 8. Direct runner, slice check, forward fragment

8.1 Deploy the new runner binary with the unchanged unit first, so a failed
deploy rolls back to a binary that understands its unit. Build it on the server
per [runner-install.md](runner-install.md#4-install-the-runner-binary), then as
`ubuntu`:

```sh
cd /srv/build/src/motor
target/release/motor-runner --version
script/runner/deploy.sh /srv/build/src/motor/target/release/motor-runner main
```

8.2 Land the reviewed change to `script/runner/units/motor-runner.service`
(the source of truth for runner flags). Its only change is in `ExecStart`:

```diff
-ExecStart=/home/ubuntu/.cargo/bin/motor-runner serve --instance main --max-weight 80 --job-network veth --windows-cpus 12-15 --windows-slot-lock /srv/build/motor-windows/slot.lock
+ExecStart=/home/ubuntu/.cargo/bin/motor-runner serve --instance main --linux-cpus 12-15,28-31 --max-weight 16 --job-network veth --windows-cpus 12-15 --windows-slot-lock /srv/build/motor-windows/slot.lock
```

Record its commit as `<UNIT_COMMIT>`. Sync it, then install and swap:

```sh
cd /srv/build/src/motor
git log -1 --format=%H -- script/runner/units/motor-runner.service   # <UNIT_COMMIT>
bash script/runner/install-units.sh          # answer y for motor-runner.service
systemctl --user cat motor-runner.service | grep -- '--linux-cpus 12-15,28-31 --max-weight 16'
script/runner/deploy.sh "$HOME/.cargo/bin/motor-runner" main
journalctl --user -u motor-runner.service -n 50 --no-pager   # "motor-runner serving"
```

`install-units.sh` never restarts a runner; the `deploy.sh` swap drains and
restarts it with the new flags. If it also offers `system/motor-jobs.slice`,
installing it is harmless: the drop-in still sets the limits.

8.3 Confirm the slice:

```sh
systemctl show -p MemoryMax,MemoryHigh motor-jobs.slice   # 42949672960 / 35791394134
```

8.4 Merge the forward fragment into `<CK_MOTOR_CONFIG>`, run `<SUBC_RESTART>`
and verify the restart. Fleet Linux intake resumes here.

8.5 Run a fleet-routed canary (an ordinary Linux job from a fleet repository,
not a direct `ck motor run`, with a timeout of at most 7,200 s) and check
`lease_connected` on ovh-1 as in step 7. Then watch the stop conditions below
for at least the first hour, and keep watching through the 24-hour observation
in [hosted-cutover.md](hosted-cutover.md#observe-for-24-hours).

## Stop conditions

Watch these from step 7 on. Any one of them stops the cutover: deploy the
[routing rollback](#routing-rollback) first, then diagnose. Do not resume intake
until the cause is understood.

```sh
sudo journalctl -fu motor-host -u motor-vm-helper | grep -E \
  'host_storage_low|host_storage_recovered|tenant_start_refused|tenant_start_retry_failed|tenant_memory_unavailable|heartbeat_invalid|tenant_recovery_or_initialization_failed'
cat /sys/fs/cgroup/motor-jobs.slice/memory.events       # "high" and "oom_kill" counters
cat /sys/fs/cgroup/motor-jobs.slice/memory.pressure
```

- **Storage latch held:** `host_storage_low` with no `host_storage_recovered`
  for more than 15 minutes. Follow [Latched-stall recovery](#latched-stall-recovery).
- **Tenant held start-refused:** `tenant_start_refused` for tenant 1 repeating
  (motor-host retries once per 60 s) for more than 5 minutes, or once with case
  `activation` or `post_grow_space`. The tenant advertises nothing while it is
  marked, so all hosted work for it waits. Fix the named case; see the failure
  outcomes in step 7.
- **Windows slowdown or slice throttling:** the jobs slice's `high` counter
  rising, its memory pressure sustained, an `oom_kill` in the slice, or Windows
  jobs running clearly slower than their recorded baseline.
- Also stop on any lost result or unexplained `outcome_unknown`, a
  `heartbeat_invalid`, a `tenant_memory_unavailable` that does not clear within a
  minute of the tenant being healthy, or an OOM kill of the tenant VMM.

## Latched-stall recovery

While free bytes are below the threshold (146,028,888,064 for tenant 1),
motor-host refuses every new lease and advertises `free_vcpus: 0`. It releases
only at the release point, 154,618,822,656. Nothing frees host space
automatically in this state: the guest evicts only against its own disk quota,
its trims return only blocks it has already freed, and with no leases admitted
nothing changes inside it.

1. Confirm the state: the journal shows `host_storage_low` with
   `free_bytes=... threshold_bytes=...`, and
   `sudo /root/tenant1-cutover/free.sh` is below 154,618,822,656.
2. Free host space, re-measuring after each item, in this order:
   1. Set-aside snapshots that no start can select:
      `/var/lib/motor-vm/tenants/*/suspend.resize-*` and
      `/var/lib/motor-vm/tenants/*/suspend.stale-*`
      (`sudo sh -c 'du -sh /var/lib/motor-vm/tenants/*/suspend.*-*'`, then
      `sudo rm -r -- <dir>` for each).
   2. Size bases no registered tenant uses. List them (never while a
      `resize.json` exists: its base belongs to the resize):

      ```sh
      sudo python3 - <<'PY'
      import json, pathlib
      root = pathlib.Path("/var/lib/motor-vm")
      if list(root.glob("tenants/*/resize.json")):
          raise SystemExit("a resize journal exists: finish or abort it first")
      used = set()
      for entry in pathlib.Path("/etc/motor-vm-helper/tenants").glob("*.json"):
          r = json.loads(entry.read_text())
          used.add(root / "tenant-images" / r["image_version"] / "base" / f"{r['vcpu_ceiling']}-{r['guest_ram_mib']}")
      for base in sorted(root.glob("tenant-images/*/base/*")):
          print("in use" if base in used else "UNUSED", base)
      PY
      ```

      and `sudo rm -r -- <base>` only the `UNUSED` ones.
   3. Quarantined tenants under `/var/lib/motor-vm/removed/`. The helper
      deletes each 7 days after its removal; deleting one sooner needs the
      owner's approval.
   4. The direct runner's own guard: below its soft goal (256 GiB free by
      default) it evicts idle workspaces as direct jobs are admitted, and below
      its 128 GiB floor it also evicts recently used ones. Do not delete runner
      workspaces by hand.
3. Confirm the latch clears: `free.sh` reports at least 154,618,822,656 and,
   within one 15 s heartbeat, the journal shows `host_storage_recovered` with
   `free_bytes`, `threshold_bytes` and `release_bytes`. Hosted placement then
   resumes by itself.
4. If the release point cannot be reached, keep the routing rollback in place
   and escalate to the owner with the measured figures.

## Full-disk terminal state

If the host filesystem fills despite the latch (jobs already admitted kept
writing), guest writes fail with I/O errors and the guest XFS may shut down;
tenant jobs fail, and the direct runner refuses with `runner_disk_full`.

1. Deploy the [routing rollback](#routing-rollback).
2. Stop the hosted services as in forward step 3. The tenant's clean stop may
   fail: `stop-tenant.py` then reports `recovery_required`, but the VM is
   stopped and the unclean stop is recorded. Stop the helper only once
   `running` is false.
3. Free space as in [Latched-stall recovery](#latched-stall-recovery) until
   `free.sh` reports at least the operating requirement, 223,338,299,392.
4. Run `install.sh --check`, then `--start`. The next start of the uncleanly
   stopped tenant is a recovery cold boot that runs the in-guest recovery
   check; watch for `tenant_wake_ready` or
   `tenant_recovery_or_initialization_failed`.
5. If the tenant cannot recover, its disk holds only caches and workspaces:
   with the owner's approval, follow [After the resize commit](#after-the-resize-commit)
   to reprovision it, or reprovision it fresh at the new size from its current
   record. Run the canaries again before any intake.

## Rollback

### Routing rollback

Always first, and without waiting for any size change. Merge the rollback
fragment into `<CK_MOTOR_CONFIG>`, run `<SUBC_RESTART>` and verify it. Then
submit a Linux job with a 600 s timeout and confirm it runs on the build server
(its `/proc/meminfo` shows 131,491,984 kB) and that ovh-1 logs no
`lease_connected` for it. Windows service is untouched. Hosted jobs already
placed keep their saved route until they end; check the saved job state, not
only live connections, before stopping motor-host.

### Rollback preflight

Old binaries cannot read a helper registry that carries `max_weight`. Before
restoring records, and before any return to old binaries, check that the
records to be used are keyless; this refuses the rollback for a tenant whose
saved records carry the key:

```sh
sudo python3 - /root/tenant1-cutover/saved/provision.json /root/tenant1-cutover/saved/registry.json <<'PY'
import json, sys
for path in sys.argv[1:]:
    if "max_weight" in json.load(open(path)):
        raise SystemExit(f"{path} carries max_weight: no old-binary rollback from these records")
print("keyless:", ", ".join(sys.argv[1:]))
PY
```

Before step 4 of the pre-commit order, run it again on the live
`/etc/motor-host/provision/tenant-1.json` and
`/etc/motor-vm-helper/tenants/tenant-1.json`.

### Rollback canary

Runs on the **new** binaries with the keyless old-size records, before any
return to old binaries and before fleet intake resumes. Add the temporary
`hosted_repositories: ["<CANARY_REPO>"]` entry (and `admit_direct_exec` if
needed) to the rollback config and run `<SUBC_RESTART>`, then:

```sh
T0=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ck motor run --cwd <CANARY_REPO> --vcpus 7 --timeout 600 -- 'head -1 /proc/meminfo'
ck motor run --cwd <CANARY_REPO> --vcpus 21 --timeout 600 -- 'head -1 /proc/meminfo'
```

The old 28 GiB guest advertises 26,624 MiB. The 7-vCPU canary (25,088 MiB)
must place: exit 0, a guest `MemTotal` below 29,360,128 kB (28,672 MiB) and
`lease_connected` on ovh-1 after `$T0`. The 21-vCPU canary must not place on
the tenant: no second `lease_connected`, and it ends refused before start (it
may wait up to its 30-minute queue budget) or, after `no_host_fits`, is sent to
the build server. Remove the canary entry and run `<SUBC_RESTART>`.

### Before the resize commit

Use this when the resize has not reached `committed` (its journal phase is
`started`, `base_built`, `staged`, `set_aside` or `aborting`), or no resize ran
yet. In order:

1. Keep both hosted services stopped. If
   `/var/lib/motor-vm/tenants/tenant-1/resize.json` exists, abort it and repeat
   after any crash until it reports `resize aborted`:

   ```sh
   sudo python3 script/tenant-provision/provision.py resize --abort tenant-1
   ```

   Abort restores the snapshots, removes the staged files, deletes the
   `24-79872` base only if this resize created it, verifies the old records,
   helper config and disk length, and removes the journal; free bytes return to
   their pre-resize figure. It leaves the jobs-slice drop-in. If it answers that
   the resize is committed, use [After the resize commit](#after-the-resize-commit).
2. With the new binaries' `provision.py`, switch back to the saved image:

   ```sh
   sudo python3 script/tenant-provision/provision.py switch-image tenant-1 <OLD_IMAGE>
   ```

   It rebuilds the old image's `8-28672` base if step 4.5 removed it.
3. Run the [rollback preflight](#rollback-preflight). Compare the live files
   with the saved keyless ones and write back any that differ, keeping the live
   file's mode:

   ```sh
   S=/root/tenant1-cutover/saved
   for pair in provision.json:/etc/motor-host/provision/tenant-1.json \
               registry.json:/etc/motor-vm-helper/tenants/tenant-1.json \
               helper.json:/etc/motor-vm-helper.json; do
     saved=$S/${pair%%:*} live=${pair#*:}
     sudo cmp -s "$saved" "$live" || { echo "restoring $live"; sudo sh -c "cat '$saved' > '$live'"; }
   done
   ```

   If CLOUD's ceilings were already changed (step 6), have CLOUD restore the
   saved ceilings and read them back before motor-host starts. Then start the
   new binaries (`install.sh --check`, then `--start`) and run the
   [rollback canary](#rollback-canary).
4. Only to return to the old binaries: stop motor-host, clean-stop every tenant
   and stop the helper as in forward step 3; run the preflight on the live
   records; remove the drop-in and reload; restore the saved binaries; check
   and start from a checkout of `<OLD_REV>`:

   ```sh
   sudo rm /etc/systemd/system/motor-jobs.slice.d/50-capacity.conf
   sudo systemctl daemon-reload
   S=/root/tenant1-cutover/saved
   sudo install -o root -g root -m 0755 $S/motor-host /usr/local/bin/motor-host
   sudo install -o root -g root -m 0755 $S/motor-vm-helper /usr/local/libexec/motor-vm-helper
   sudo install -o root -g root -m 0755 $S/motor-vm-loader /usr/local/libexec/motor-vm-loader
   git -C /srv/build/src/motor worktree add /srv/build/src/motor-rollback <OLD_REV>
   sudo /srv/build/src/motor-rollback/script/hosted-host/install.sh --checkout /srv/build/src/motor-rollback --check
   sudo /srv/build/src/motor-rollback/script/hosted-host/install.sh --checkout /srv/build/src/motor-rollback --start
   ```

   `capacity.json` may stay; old binaries do not read it.

### After the resize commit

Once the journal reached `committed`, or the resize completed, the resize only
rolls forward. Old records are never restored beside the grown disk or the
rewritten runner arguments, and motor-host never heartbeats an old-size tenant
while CLOUD holds the larger ceilings. The tenant's data is lost: it is
reprovisioned empty at the old size. In order:

1. Deploy the [routing rollback](#routing-rollback). Drain and stop the hosted
   services as in forward steps 2 and 3, and keep motor-host stopped.
2. Run the [rollback preflight](#rollback-preflight) on the saved records.
3. Quarantine plan. Measure what the grown disk occupies, then remove the
   tenant into quarantine (never `--purge-now` without the owner's approval):

   ```sh
   sudo stat -c '%b %B %n' /var/lib/motor-vm/tenants/tenant-1/disk.img   # allocated = %b × %B
   sudo python3 script/tenant-provision/provision.py remove-tenant tenant-1 --dry-run
   sudo python3 script/tenant-provision/provision.py remove-tenant tenant-1
   sudo /root/tenant1-cutover/free.sh
   ```

   The quarantine folder `/var/lib/motor-vm/removed/tenant-1-<unix seconds>/`
   stays on the same filesystem for 7 days, so removal frees nothing until the
   helper purges it. The fresh tenant needs 255,550,554,112 bytes free with the
   old image's `8-28672` base present, or 286,152,196,096 without it; if
   `<OLD_IMAGE>` does not list `sparse-v1`, its 100 GiB disk is fully allocated,
   so also require 107,374,182,400 more (362,924,736,512 or 393,526,378,496;
   `provision.py` does not count that allocation itself). Reclaim, in order:
   the now-unused `<NEW_IMAGE>` bases `24-79872` (about 84 GB) and `8-28672`,
   using the unused-base listing in
   [Latched-stall recovery](#latched-stall-recovery); then the set-aside
   snapshots. Purging the quarantine early needs the owner's approval.
4. Reprovision fresh at the old size and image, without `max_weight`, and
   restore the saved helper config. `remove-tenant` removed the provisioning
   record and `provision.py` does not write it, so install the saved one first:

   ```sh
   S=/root/tenant1-cutover/saved
   sudo install -o root -g root -m 0644 $S/provision.json /etc/motor-host/provision/tenant-1.json
   sudo python3 script/tenant-provision/provision.py --config /etc/motor-host/provision/tenant-1.json
   sudo sh -c "cat $S/helper.json > /etc/motor-vm-helper.json"
   sudo cmp $S/tenants.json /etc/motor-host/tenants.json && echo 'bindings as saved'
   ```

5. Have CLOUD restore the saved ceilings with `<CLOUD_SET_CEILINGS>` and read
   them back with `<CLOUD_READ_CEILINGS>`; they must equal
   `saved/cloud_ceilings.txt`. motor-host stays stopped until then.
6. Only then start the new binaries (`install.sh --check`, then `--start`), run
   the [rollback canary](#rollback-canary), and resume intake on the routing
   rollback. To return to old binaries, follow step 4 of
   [Before the resize commit](#before-the-resize-commit).

### Direct runner revert

Returns the direct runner to `--max-weight 80` on all CPUs. Do it only after
the routing rollback and once tenant 1 is back at its old cpuset (or stopped):
at 80 the runner uses the threads the 24-vCPU tenant runs on. Land a reviewed
`git revert <UNIT_COMMIT>`, sync it, then as `ubuntu`:

```sh
cd /srv/build/src/motor
bash script/runner/install-units.sh          # answer y for motor-runner.service
systemctl --user cat motor-runner.service | grep -- '--max-weight 80'
script/runner/deploy.sh "$HOME/.cargo/bin/motor-runner" main
```

The live binary accepts both unit forms, so only the unit changes. To return
to the pre-cutover runner binary as well, deploy the `<live>.rollback-<UTC>`
that `deploy.sh` kept in step 8.1, after this revert.

## Malformed resize journal

A `resize.json` that cannot be read, is not JSON, has missing or unknown keys,
another schema version or an unknown phase, or names paths the resize would not
have created makes `resize` and `resize --abort` refuse with
`malformed resize journal ...`, and the tenant stays start-refused. Recover by
hand with both services stopped, following "Malformed resize journal" in
[`script/tenant-provision/README.md`](../script/tenant-provision/README.md).
The old bytes are the saved copies in `/root/tenant1-cutover/saved/`
(`provision.json`, `registry.json`, `helper.json`); the new ones are what the
resize command above would publish. In short:

1. Compare the three live files with the saved copies, and the `disk.img`
   length (`sudo stat -c %s`) with 107374182400 and 644245094400.
2. All three files and the length still old: restore the old state (move the
   newest `suspend.resize-*` back to `suspend/` if `suspend/` is empty, delete
   `runner-args.staged` and `resize-staged/`, delete the `24-79872` base only if
   no registry entry names it), then continue with step 2 of
   [Before the resize commit](#before-the-resize-commit).
3. Any of them new: the resize committed. Finish it as the README says (all
   three files at the new size, `truncate -s 644245094400 disk.img`, and the
   staged runner arguments), then continue forward from step 5.
4. Move `resize.json` out of the tenant home into `/root/tenant1-cutover/`
   for the incident record, and run `install.sh --check` before any start.

## Rehearsals

Rehearse every rollback on a throwaway tenant, `tenant-rehearsal`, on ovh-1
after forward step 4.2 and before step 4.3, with both hosted services stopped
and tenant 1 untouched. Any rehearsal that does not end in its expected state
stops the cutover: follow [Before the resize commit](#before-the-resize-commit)
from step 4 to return to the old binaries' slice. Never start
motor-host while the throwaway tenant is registered. It uses tap index 2 (check
that no registry entry uses it), cpuset `0-1,16-17` (outside tenant 1's
`8-11,24-27` and the Windows CPUs), 2 vCPUs, 4 GiB RAM and a 10 GiB disk, and
a subject no account uses. Its resize keeps the cpuset: a cpuset change also
rewrites `/etc/motor-vm-helper.json`, whose slots follow tenant 1.

Its record, from tenant 1's saved record:

```sh
S=/root/tenant1-cutover/saved
sudo python3 - $S/provision.json <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
c.update(tenant="tenant-rehearsal", vm="vm-rehearsal", sub="acct:00000000000000000000000000",
         tap_index=2, cpuset="0-1,16-17", vcpu_ceiling=2, guest_ram_mib=4096,
         disk_quota_bytes=10737418240, image_version="<OLD_IMAGE>")
c.pop("max_weight", None)
open("/root/tenant1-cutover/rehearsal.json", "w").write(json.dumps(c, indent=2) + "\n")
PY
```

A crash is simulated by stopping `provision.py` at an exact point with
`os._exit`, which skips every cleanup as a power loss would. Save this driver:

```sh
sudo tee /root/tenant1-cutover/crash.py >/dev/null <<'PY'
#!/usr/bin/env python3
"""crash.py resize|abort <point>: run the rehearsal resize or abort and exit
abruptly when provision.py reaches <point> (for example phase:base_built,
step:set_aside, phase:aborting)."""
import importlib.util, os, sys

spec = importlib.util.spec_from_file_location(
    "provision", "/srv/build/src/motor/script/tenant-provision/provision.py")
provision = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provision)
command, point = sys.argv[1], sys.argv[2]

def hook(reached):
    if reached == point:
        print("crash at", reached, flush=True)
        os._exit(137)

if command == "resize":
    provision.resize("tenant-rehearsal", {
        "vcpu_ceiling": 4, "guest_ram_mib": 8192,
        "disk_quota_bytes": 21474836480, "max_weight": 16, "idle_suspend_s": 0}, hook=hook)
else:
    provision.resize_abort("tenant-rehearsal", hook=hook)
print("finished without reaching", point)
PY
sudo chmod 0700 /root/tenant1-cutover/crash.py
```

Setup, once (the provisioner and the image are `<NEW_REV>`'s and
`<OLD_IMAGE>`):

```sh
cd /srv/build/src/motor
P="sudo python3 script/tenant-provision/provision.py"
sudo install -o root -g root -m 0644 /root/tenant1-cutover/rehearsal.json /etc/motor-host/provision/tenant-rehearsal.json
$P --config /etc/motor-host/provision/tenant-rehearsal.json
sudo install -d -m 0700 /root/tenant1-cutover/rehearsal-saved
sudo cp -p /etc/motor-host/provision/tenant-rehearsal.json /etc/motor-vm-helper/tenants/tenant-rehearsal.json \
  /etc/motor-vm-helper.json /root/tenant1-cutover/rehearsal-saved/
```

After each rehearsal below, verify: the throwaway's record and registry and
`/etc/motor-vm-helper.json` equal the copies in `rehearsal-saved/` (`sudo cmp`),
the disk length is 10737418240, no `resize.json` exists, the `4-8192` base is
gone unless another tenant uses it, `free.sh` is back within a few MiB of its
figure before the resize, and the rollback preflight passes on the live
records. Record each result.

1. **Crash after `switch-image`.** `$P switch-image tenant-rehearsal <NEW_IMAGE>`,
   then stop there and follow the pre-commit order: no journal, so
   `$P switch-image tenant-rehearsal <OLD_IMAGE>`, then the file comparison of
   step 3 against `rehearsal-saved/`.
2. **Crash before commit.** Switch to `<NEW_IMAGE>` again, then for each of
   `phase:base_built`, `step:set_aside`:

   ```sh
   sudo /root/tenant1-cutover/crash.py resize phase:base_built   # then step:set_aside
   $P resize --abort tenant-rehearsal
   ```

   and for a crash mid-abort:

   ```sh
   sudo /root/tenant1-cutover/crash.py resize step:set_aside
   sudo /root/tenant1-cutover/crash.py abort abort:staged
   $P resize tenant-rehearsal --vcpu-ceiling 4 --guest-ram-mib 8192 \
     --disk-quota-bytes 21474836480 --max-weight 16 --idle-suspend-s 0   # must refuse: being aborted
   $P resize --abort tenant-rehearsal                                    # completes
   ```

   Each ends with the pre-commit order's `switch-image` back to `<OLD_IMAGE>`
   and the verification.
3. **Completed resize.** Switch to `<NEW_IMAGE>`, run the full resize with the
   command shown in the last block, then follow
   [After the resize commit](#after-the-resize-commit) steps 2–4 for the
   throwaway: preflight on `rehearsal-saved/`, `remove-tenant tenant-rehearsal`,
   reinstall `rehearsal.json` as `/etc/motor-host/provision/tenant-rehearsal.json`
   and `$P --config /etc/motor-host/provision/tenant-rehearsal.json`, and write
   back `rehearsal-saved/motor-vm-helper.json` as `/etc/motor-vm-helper.json`.
   The reprovisioned tenant has a new, empty disk.

Finally remove the throwaway with
`$P remove-tenant tenant-rehearsal --purge-now`: it never held customer data,
so its quarantine is not kept. Delete its unused bases with the listing in
[Latched-stall recovery](#latched-stall-recovery), and confirm
`/etc/motor-vm-helper.json` equals tenant 1's `saved/helper.json`.
