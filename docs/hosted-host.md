# Provision a hosted host

This turns the dedicated Linux build server into a hosted-runners host: the
root `motor-vm-helper`, the unprivileged `motor-host` TLS agent, their
configs and keys, one tenant VM for our fleet's account, and one public TLS
port. The existing SSH runner keeps running beside it. Broker-side enrollment
belongs to CLOUD; this procedure prints the facts CLOUD registers.

The host id and the tenant's broker subject (`acct:<ULID>`) are installer
arguments. They are not in this public repository; after installing they live
only in the server's `/etc/motor-host/host.json` and
`/etc/motor-host/provision/tenant-1.json`. Below, `<host-id>` and
`<tenant-sub>` stand for them.

Everything here runs **on the server**, as `ubuntu` with `sudo`, from the
checkout `script/runner/sync.sh` maintains at `/srv/build/src/motor`.

## What gets installed

| Path | Owner, mode | Source |
| --- | --- | --- |
| `/usr/local/bin/motor-host` | root, 0755 | `cargo build --release -p motor-host` |
| `/usr/local/libexec/motor-vm-helper` | root, 0755 | `cargo build --release -p motor-vm-helper` |
| `/usr/local/libexec/motor-vm-loader` | root, 0755 | static musl build of `motor-vm-loader` (the path the helper's fixture config and [linux-microvm.md](linux-microvm.md#motor-vm-loader) use) |
| `/etc/systemd/system/motor-vm-helper.service` | root, 0644 | `crates/motor-vm-helper/systemd/` |
| `/etc/systemd/system/motor-host.service` | root, 0644 | `crates/motor-host/systemd/` |
| `/etc/motor-host/host.json` | root, 0644 | `hosted_host.py host` |
| `/etc/motor-vm-helper.json` | root, 0644 | `hosted_host.py helper` |
| `/etc/motor-host/provision/tenant-1.json` | root, 0644 | `hosted_host.py tenant`: `script/hosted-host/tenant-1.json` plus the subject |
| `/etc/motor-host/tenants.json` | root, 0644 | created as `{}`; `provision.py` adds the tenant |
| `/etc/motor-host/keys/{host-signing.key,tls.crt,tls.key}` | motor-host, 0600 | `motor-host keygen`, only when all three are absent |
| `/var/lib/motor-host/` | motor-host, 0700 | the agent's durable journal |

`motor-host` is a system account with no home and no login shell. It is the
only uid in the helper's `allowed_uids`, and its group is the helper socket's
group. Its unit runs with no capabilities, `NoNewPrivileges`, a private `/tmp`
and devices, and `ProtectSystem=strict`: only `/var/lib/motor-host` and its
runtime directory `/run/motor-host` (the admin socket) are writable. It starts
after, and requires, `motor-vm-helper.service`.

The host config listens on `0.0.0.0:8443` (IPv4) with resolvers 1.1.1.1 and
9.9.9.9 and storage `/var/lib/motor-vm`, and has no `fake_broker_url`.

### Sizes beside the runner

The server has 16 cores / 32 threads (thread *n* and *n*+16 share a core); the
Windows VM owns 12-15 and 28-31. The Linux runner runs at `--max-weight 80` on
the rest and is busy, so tenant 1 starts small:

- `cpuset` `8-11,24-27`: both threads of four cores, none of them Windows's.
  These threads are shared with the runner; the tenant takes CPU only while it
  runs a lease, because its CPU quota follows the leased vCPUs (one while idle).
- `vcpu_ceiling` 8, the size of that cpuset.
- `guest_ram_mib` 28672. A lease for `v` vCPUs lets its job use
  `memory_max(v) = max(16 GiB, 3.5 GiB × v)`, which is 28 GiB at 8 vCPUs; a
  16 GiB guest would let a full-size job be killed for memory the broker
  granted. (16 GiB of guest RAM is enough only for a vCPU ceiling of 4.) The
  size is a cap, not an up-front allocation: guest RAM is file-backed and
  allocated as the guest touches it, and an idle tenant is snapshotted to disk
  and stopped after `idle_suspend_s` (600 s).
- `disk_quota_bytes` 107374182400 (100 GiB), so the in-VM runner's compile
  cache is 25 GiB (a quarter of the disk, at most 200 GiB). The tenant disk is
  on `/srv/build`, which the SSH runner also uses: its disk guard refuses jobs
  below its free-space floor (128 GiB by default) and evicts idle workspaces
  and caches below its eviction threshold (twice the floor by default). A
  tenant disk cannot be resized later, only reprovisioned.
- `tap_index` 1 (tenant uid 400001), `io_weight` 100, the provisioner's other
  defaults.

`provision.py` checks bytes before it allocates anything. It needs the disk,
a suspend snapshot (guest RAM), `host_floor_bytes` (128 GiB) and, for a new
size base, guest RAM plus 512 MiB free. On a host that also runs
`motor-runner serve`, it also reads each runner's `--free-space-floor-bytes`
and state root from its live arguments. If a runner's state is on the same
filesystem as `/var/lib/motor-vm`, the free space left after the disk, snapshot
and base must stay at or above that runner's refusal floor, where the runner
starts refusing jobs. Otherwise provisioning refuses with "would leave X GiB
free ... below motor-runner pid N's refusal floor Y GiB". Here the tenant
consumes about 156.5 GiB, so with the default 128 GiB floor it needs about
284.5 GiB free; the provisioner's own `host_floor_bytes` check needs the same.
Between the floor and the eviction threshold the runner evicts idle
workspaces, least recently used first, sooner than it would without the
tenant. Pass `--tenant-disk-quota-gib` at install to change the disk size.

The cutover's other requirement, `vcpu_ceiling` equal to the runner's
`--max-weight`, is deliberately not met: that applies when the hosted route
replaces the SSH runner, not while it runs beside it.

The helper's job-slot settings are inert: `motor-host` sends only tenant
operations, but the helper config requires one slot. It uses slot uid 290000
and a one-uid job range starting at 300000, the values the tenant-image
qualification harness (`script/tenant-image/qualify.py`) ran the helper with
on this server.

## Firewall

`script/runner/setup.sh` refuses every inbound allow except SSH. It has one
named exception: the host agent's port, IPv4 TCP only. `install.sh --start`
adds it and `install.sh --stop` deletes it, written exactly as

```sh
sudo ufw allow in proto tcp from any to 0.0.0.0/0 port 8443 comment motor-host-agent
```

and accepted only when the port is named. `script/runner/inbound_policy.py`
holds the check (setup.sh sends it to the server and runs it): with
`--host-agent-port 8443` it accepts that rule and a TCP listener on
`0.0.0.0:8443`; an IPv6 rule or listener for the port, a rule without the
comment or any other inbound allow is refused. When re-running `setup.sh` on
this server while the host is in service, set `RUNNER_HOST_AGENT_PORT=8443`;
without it setup.sh refuses the rule.

The check reads `ufw status verbose`. setup.sh used to read plain `ufw status`,
which never prints the `ALLOW IN` its check looked for, so it could not see an
extra rule.

## Procedure

1. From the operator machine, sync the checkout: `script/runner/sync.sh`.
2. On the server, check the prerequisites: the private `/var/lib/motor-vm`
   storage view ([linux-microvm.md](linux-microvm.md#install)), Firecracker and
   jailer 1.17.0 in `/usr/local/lib/firecracker/1.17.0/`, the qualified tenant
   image in `/var/lib/motor-vm/tenant-images/tenant-20261020-main-806a6d3-20261020T021255Z/`,
   and the `x86_64-unknown-linux-musl` Rust target. The installer verifies all of
   them and stops at the end with a count of problems if one is missing.
3. Build and install. The build runs as the checkout's owner, with
   `/srv/build/runner.env`, into `/srv/build/target`:

   ```sh
   cd /srv/build/src/motor
   sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --build \
     --host-id <host-id> --tenant-sub <tenant-sub>
   ```

   It creates the `motor-host` account, installs the binaries, shows each new or
   changed config and unit and installs it on `y`, generates the keys if none
   exist and prints the enrollment facts. It neither enables nor starts a unit
   and does not open the port. The public IPv4 address is discovered from the
   one global address on the machine; pass `--public-ip` if there is not
   exactly one. `--port` (default 8443) and `--tenant-disk-quota-gib` (default
   100) override the defaults. A declined change or any failed check exits
   nonzero.
4. Send CLOUD the enrollment block. It is five labelled lines in
   `register-host` argument order, then the filled-in command:

   ```text
   host_id: <host-id>
   public_key_hex: <64 lowercase hex: the host signing key's Ed25519 public key>
   endpoint: <public IPv4>:8443
   endpoint_spki_sha256: <43 characters: base64url SHA-256 of the TLS certificate's SubjectPublicKeyInfo>
   platform: linux-x64
   register-host <host-id> <public_key_hex> <public IPv4>:8443 <endpoint_spki_sha256> linux-x64
   ```

   Reprint it at any time with
   `sudo runuser -u motor-host -- motor-host show-enrollment /etc/motor-host/host.json --endpoint <public IPv4>:8443`.
   It refuses a config with `fake_broker_url`, an endpoint that is not a public
   IPv4 address with the listen port, and a TLS key that does not match the
   certificate.
5. Provision tenant 1 (this boots a size base once and allocates its disk):

   ```sh
   sudo python3 /srv/build/src/motor/script/tenant-provision/provision.py --config /etc/motor-host/provision/tenant-1.json
   ```

   To change an existing tenant to a new image without recreating its disk, run
   the following command from the checkout at `/srv/build/src/motor`:

   ```sh
   sudo python3 /srv/build/src/motor/script/tenant-provision/provision.py switch-image tenant-1 <image-version>
   ```

   `switch-image` takes the shared `/var/lib/motor-vm/provision.lock` and refuses
   unless both `motor-host.service` and `motor-vm-helper.service` are inactive; it
   does not stop or start either service. Stop them first with

   ```sh
   sudo systemctl stop motor-host.service motor-vm-helper.service
   ```

   Stopping the helper is the clean stop: it has each running tenant's guest
   drain, `sync -f /tenant` and freeze (bounded at 15 s) before it stops the
   VM, and only then exits. The guest refuses that while its runner has a job
   queued or running, so stop when the tenant is idle; a tenant that cannot be
   stopped cleanly is recorded as unclean and its next start runs the in-guest
   recovery check. `journalctl -u motor-vm-helper` shows `tenant tenant-1: clean
   stop took N ms` or the reason it was not clean. The stop can take up to
   `TimeoutStopSec` (420 s) when a suspend is in progress. If the tenant's
   last stop was not clean, `switch-image` prints a warning saying so.
   It verifies the image with the hosted
   image inventory command, builds the tenant's size-specific base if needed,
   and updates the helper registry and `/etc/motor-host/provision/tenant-1.json`
   together. An image-mismatched suspend snapshot is moved to a timestamped
   sibling directory rather than deleted, and its new path is printed. The
   command prints the old and new image versions and an exact rollback command.
   A repeated switch to the same image is safe. `install.sh --check` treats the
   installed image version as mutable state but still checks the other tenant
   fields against the checkout template and confirms the registry agrees. The
   command never starts the host; run the existing `install.sh --start` only
   after `install.sh --check` and a review of its results.

   If `switch-image` refuses with `provisioning record and helper registry
   image versions differ`, the image was changed outside `switch-image`. The
   registry is what the helper boots. When every other field agrees, adopt its
   image into the provisioning record, then switch:

   ```sh
   sudo python3 /srv/build/src/motor/script/tenant-provision/provision.py repair-from-registry tenant-1
   ```

   It prints `<record>: image_version <old> -> <registry image> (from
   <registry>); no other field changed`, or that nothing changed. It refuses,
   changing nothing, if the registry's image is missing or fails the inventory
   check, or if any other field differs. A hand edit of the record is not a
   substitute: `install.sh --check` requires the record to be byte-identical to
   its own rendering (`hosted_host.py tenant`), so reformatting the file or
   changing its mode fails the check.

6. Verify without changing anything. `--check`, `--start` and `--stop` read
   the host id, subject, port and disk size from the installed configs:

   ```sh
   sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --check
   ```

7. Only after CLOUD confirms the registration, start the host. The broker
   places leases on the host from its first signed heartbeat, so this is the
   step that puts the host into service:

   ```sh
   sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --start
   ```

   `--start` runs every `--check` check and stops if any fails. It also refuses
   while `tenants.json` has no tenant. Then it opens the firewall exception,
   enables and starts the helper, waits for its socket, and enables and starts
   `motor-host`. Watch it with `journalctl -fu motor-host`. Only `--start`
   enables the units, because an enabled unit would start on the next reboot
   and make the host placeable.

## Roll back

Ask CLOUD to stop placing on the host, then:

```sh
sudo script/hosted-host/install.sh --checkout /srv/build/src/motor --stop
```

`--stop` disables and stops `motor-host` first, so it sends no further
heartbeat. It then disables and stops the helper, which stops the tenant VM
cleanly, as described for `switch-image` above; the disk is kept. If the stop
was not clean, the next start runs the in-guest recovery checks. It
deletes the `<port>/tcp ... # motor-host-agent` firewall rule and checks that
the server is back to SSH only (`inbound policy: SSH only`). It keeps the keys,
journal, configs and tenant disk; never delete a tenant disk or the journal as
part of a rollback.

To return to service, run `--check`, then `--start`.

## Remove a departed customer's tenant

The public privacy page promises that a departed customer's tenant VM disk is
deleted within 30 days of the account closing. Removal is two steps: the
tenant's data is first moved into a root-only quarantine folder,
`/var/lib/motor-vm/removed/<tenant>-<unix seconds>/`, and `motor-vm-helper`
deletes each such folder 7 days after the removal (an hourly purge;
`QUARANTINE_S` in `crates/motor-vm-helper/src/removal.rs`). The folder holds
the tenant home (`home/`: disk, generation, suspend snapshot, set-aside
`suspend.stale-*` snapshots, stop markers) and copies of its records
(`records/registry.json`, `records/provision.json`, `records/binding.json`).
The shared size base under the image is not per tenant and is kept.

**Automatic.** The broker lists closed accounts in its heartbeat reply as
`closed_subjects` (`["acct:<ULID>", ...]`): always present, sorted, and empty
unless a tenant the heartbeat reports (a warm entry, or the owner of a running
job) belongs to a closed account. Closure is permanent. A reply without the
member, from a broker older than account closure, closes nothing. The broker
also refuses the closed account's new leases (403 `account_closed`), while its
placed jobs run to their end. motor-host maps each subject to a
tenant only through its binding map (`/etc/motor-host/tenants.json`), ignores
subjects bound to no tenant, records the closure in its journal and refuses
new jobs for the tenant (`auth`). Once the tenant has no open lease or
connection and its last job ended at least 24 hours ago (the runner's result
retention), motor-host stops the VM cleanly and asks the helper to
`remove_tenant`. The helper takes the tenant name only: it reads the tenant's
own registry entry, refuses an unknown tenant or a VM that is still running,
reconciles the firewall over the remaining tenants (dropping the tenant's nft
chains, counter and UFW route grant), deletes its tap, drops its binding,
provisioning record and registry entry, and moves its home into quarantine.
A failed removal is retried with backoff from one minute to six hours. Once 7
days have passed since the closure with the tenant still present,
`tenant_removal_overdue` is logged once as an error, and each later failed
attempt logs `tenant_removal_failed_overdue` as an error.
Logs: `tenant_account_closed`, `tenant_removed` (with the quarantine folder).

**By hand.** With both services stopped, as for `switch-image`:

```sh
sudo systemctl stop motor-host.service motor-vm-helper.service
sudo python3 script/tenant-provision/provision.py remove-tenant <tenant> --dry-run
sudo python3 script/tenant-provision/provision.py remove-tenant <tenant>
sudo systemctl start motor-vm-helper.service motor-host.service
```

`--dry-run` lists every file, record, binding and network object it would
remove and changes nothing. `remove-tenant` refuses while either service is
active (no VM can run with the helper stopped, and no lease or session exists
with motor-host stopped), holds the provision lock, quarantines exactly what
the helper's removal does (`script/tenant-provision/removal-layout.json` pins
both) and deletes the tap. The helper's startup reconciliation then drops the
firewall exemption and UFW route grant, because it keeps only registered
tenants. `--purge-now` deletes at once instead of quarantining. Other tenants
are untouched.

**Undo, within the 7 days.** With both services stopped, move
`removed/<tenant>-<time>/home` back to `/var/lib/motor-vm/tenants/<tenant>`,
copy `records/registry.json` to `/etc/motor-vm-helper/tenants/<tenant>.json`
(mode 644) and `records/provision.json` to
`/etc/motor-host/provision/<tenant>.json`, add the entry in
`records/binding.json` back to `/etc/motor-host/tenants.json`, then start the
services; the helper recreates the tap and firewall rules on the tenant's next
start. A tenant whose account is still closed at the broker is removed again,
so undo only a removal made in error. To re-provision instead, run
`provision.py --config` with the saved provisioning record after moving the
old home aside.

## Upgrading a helper that predates the clean stop

A helper built before the clean service stop dies on SIGTERM and wrote no
`running.json` marker, so the restart onto the new one cuts a running tenant's
power and nothing detects it. Before restarting it, check for a running tenant
VM with `pgrep -af 'firecracker.*--id tenant-'`. If one is listed, either wait
until the tenant is suspended (the process is gone), or record the stop as
unclean first so the next start runs the in-guest recovery check:

```sh
echo '{}' | sudo tee /var/lib/motor-vm/tenants/tenant-1/running.json
```

Do not write the marker for a tenant that is not running: it discards the
suspend snapshot and runs recovery for nothing.
