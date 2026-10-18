# motor-host

Unprivileged TLS admission for tenant VMs. The helper owns VM paths, disks,
cgroups and taps; this process sends only registered tenant names and bounded
helper operations. Run `motor-host <config.json>` as the user authorized by the
helper. It requires a PEM certificate and PEM private key, not a public-CA
certificate: clients compare the certificate's SubjectPublicKeyInfo (SPKI),
which identifies its public key, with the SHA-256 pin supplied in the broker's
lease response.

Configuration is a closed JSON object with these required fields:

- `host_id`, `listen` (a socket address), `certificate`, `private_key`;
- `journal_dir` (normally `/var/lib/motor-host`);
- `helper_socket` (normally `/run/motor-vm-helper.sock`);
- `bindings` (normally `/etc/motor-host/tenants.json`), the provisioner's VM to
  `{tenant, sub}` map;
- `registry_dir` (normally `/etc/motor-vm-helper/tenants`), readable root-managed
  helper registry files supplying ceilings, sizes, tap indices and idle policy;
- `resolvers`, an array of public IPv4 strings matching provisioning;
- `storage`, the filesystem path whose available bytes cover suspend snapshots;
- `admin_socket`, in a private directory writable by the host user.

The agent derives the guest address and gateway from the registered tap index.
Guest hostnames use that index, and machine IDs derive from host, subject and VM
identity. Fresh starts receive `attach_tenant`; restored guests receive only
`post_restore`, with fresh host time and entropy. Adoption of an already initialized
generation skips initialization and rebuilds its CPU quota from the journal.

## Admission and durability

The version-1 lease handshake (`lease_hello`, requester challenge, and result)
and its fixed wire formats and cryptography come from `motor-lease`. The host
fetches the pinned issuer's JWKS at startup, every ten minutes, and at most once a
minute after authentication fails. An empty or failed JSON Web Key Set (JWKS)
fetch preserves the last successfully fetched broker public-key set. No public
keys means no admission. Tenant binding and requester-key binding precede
the challenge; signature verification precedes every wake and disk operation.

Job tokens follow the mirrored CLOUD contract and use the full `linux-x64`
platform name, lowercase UUIDv7 job IDs and nullable placement VM field. A null
or unknown VM has no provisioned tenant binding and is refused with `auth`;
the host does not invent a placement for it.

The journal is an atomically replaced, file-and-directory-fsynced snapshot. Its
lease states, current token issue time, delivered flag, nonce replay set,
acknowledged lifecycle event numbers and highest client-output sequence numbers
survive restart. A failed
journal write authorizes neither relay delivery nor an acknowledgement. The host
journals each higher output sequence before writing it to a client.

Every run token checks the tenant ceiling, including while the VM is warm. RAM
kill caps are not summed for admission. Strictly newer pre-admission tokens close
the older connection and wait for its lifecycle refusal before taking over its
single share. An admission event received while replacement or expiry is pending
keeps the original job admitted: the replacement is refused and no zero-expiry
settlement is created.
Disconnection alone never proves that a delivered lease did not start.

Connections wait on retained watch notifications for admission, refusal,
replacement and closure, not a periodic mutex poll. A separate expiry timer
bounds pre-admission waiting and is disarmed after admission. Capacity, host-full
and expiry refusals keep their actual wire reasons; a replacement whose original
job was admitted gets `auth`. Missing lease records are checked without panicking
under the shared host lock.

No-start endings create idempotent `zero_usage` journal entries for the signed
usage/outbox consumer. Admitted/started end timestamps and `stop_pending` are
retained for the usage/watchdog layer; this admission package does not send
broker usage requests or implement terminal billing/delivery grace. Lifecycle
and relay handling expose that durable state for that layer rather than treating
a network disconnect as a settlement.

CPU quota remains one vCPU until guest initialization replies, then uses the
current share (at least one). Started leases past their runtime cap leave the
share even after restart. Idle suspension requires a runner idle event, no
Reserved/admitted/started lease, no relay or prelude, continuous idle time and
available bytes at least `guest_ram_mib * 1048576 + host_floor_bytes`. The helper
rechecks space and quiesces the guest before snapshotting.

Helper work is serialized per tenant, never while holding the shared host-state
mutex. Tick, lifecycle and connection-cleanup paths plan quota updates under that
mutex, then apply them outside it. Each quota plan carries a host-side generation;
the per-tenant worker coalesces old plans to the latest desired value and records
the applied generation. These counters are independent of disk generations and
do not add arguments to the privileged `set_cpu_max` RPC.

Suspend candidates are rechecked after the free-space read and after obtaining
the tenant's operation lock. A `suspending` marker makes new admissions for that
tenant return retryable `capacity` without creating a lease. Other tenants can
still admit and close connections during the snapshot. Completion rechecks
idleness and preserves new activity; a successful snapshot clears VM readiness,
so a later connection restores it. Async connection cleanup runs in the blocking
pool rather than issuing helper calls from a destructor on a runtime thread.

The mode-0600 admin socket uses four-byte big-endian length-prefixed JSON. A
request combines `tenant` with one guest control operation: `drain` with
`deadline_s` and `exit_after`, `resume`, `cancel` with `job_id`, or `status`. Only
those typed requests are forwarded on port 54; none is accepted on the TLS job
path by the host admin service.

## Verification

`cargo test -p motor-host` runs the portable admission, durable-journal, TLS,
framed relay and helper descriptor tests. `tests/it/` is folded into library tests.
Actual Firecracker restore, cgroup enforcement, root-owned registry permissions
and free-space accounting on the operator filesystem require the dedicated
hosted VM qualification lane; portable tests are not that qualification.
