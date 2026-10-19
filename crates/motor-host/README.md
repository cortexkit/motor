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
- `host_signing_key`, a host-private file containing a raw 32-byte Ed25519 seed
  registered with the broker (separate from the TLS key);
- `journal_dir` (normally `/var/lib/motor-host`);
- `helper_socket` (normally `/run/motor-vm-helper.sock`);
- `bindings` (normally `/etc/motor-host/tenants.json`), the provisioner's VM to
  `{tenant, sub}` map;
- `registry_dir` (normally `/etc/motor-vm-helper/tenants`), readable root-managed
  helper registry files supplying ceilings, sizes, tap indices and idle policy;
- `resolvers`, an array of public IPv4 strings matching provisioning;
- `storage`, the filesystem path whose available bytes cover suspend snapshots
  (use the root-trusted `/var/lib/motor-vm` bind view, not `/srv/build/vm`);
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

No-start endings create idempotent `zero_usage` journal entries. The billing
owner consumes them alongside real terminal frames, using host observations only.
A lifecycle connection starts with an additive `event_replay {through_seq}`
watermark; existing event and acknowledgement frames are unchanged. Events at or
below the watermark are replay: started is stamped at host restart and terminal
at the previous `last_alive`. Without a watermark, all events are conservatively
replay. A wholly unobserved run settles zero, never guest-reported elapsed time.

At the runtime cap the watchdog journals a cancel and removes the CPU share.
After another 60 seconds without a runner result on the job relay (port 52) or
a terminal lifecycle event on the event stream (port 53), the watchdog records
the job as lost. Receiving the job-relay result prevents that lost outcome. If
its lifecycle event is missing for 30 seconds, the host instead uses the receipt
time of the job-relay result as the end. An unstarted lease is cancelled at token
issue time plus six minutes.
Lost jobs have a stored empty output truncation marker and unsequenced
`outcome_unknown` terminal, followed by the immutable `hosted_terminal` billing
record. The synthesized empty output uses exec-remote/v1's `bytes` field. Reattaches replay
these records without waking a guest. A pending VM stop rejects new runs with
`capacity`, lets other jobs finish, then stops the VM without a snapshot.

Real terminals are held until their integer billing record is durable, then
written before `hosted_terminal`. Usage is enqueued after that write or after a
60-second delivery grace. Detached results are retrieved through a host-only
attach, never by resubmitting Run. Different broker settlement figures and final
409 replies log `usage_billing_diverged` without changing the caller's figure.

One broker sender signs heartbeats every 15 seconds and on lifecycle change, and
drains the durable usage outbox. Its counter is fsynced before signing, seeded at
`max(persisted + 1, unix_ms * 1000)` on restart. Retries preserve body bytes and
use fresh timestamps, counters and lowercase-hex signatures from `motor-lease`.
Only one signed request is in flight. `usage_conflict`, `job_settled` and
`job_unknown` are final reconciliation errors; other failures retry. After 30
days an entry is archived under `journal_dir/outbox-expired` and the queue moves
on. A 401 emits a key-retirement alert for an operator to check clock reversal.

The read-only `tenant_metrics` helper operation samples the VM's
`memory.current` and its own nftables forward-rule byte counter at start, end and
periodic heartbeats. VM-lifetime `memory.peak` cannot be scoped to one lease
without resetting shared state, so memory is a sampled peak: spikes between the
15-second samples can be missed. Both measurements are null after overlap with
another Reserved, Admitted or Started lease; otherwise they are integer values.
Missing initial samples are not fabricated as zero. All helper and broker I/O
runs outside the host-state mutex.
The helper preserves each registered tenant's named egress counter across other
tenants' enable/disable transactions and across helper reconciliation, so a solo
job's byte delta does not reset when another tenant starts.

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

The image qualification must also exercise an unclean stop during a Git write:
verify that the durable host `unclean-stop.json` survives agent/helper restart;
that new work is refused until the guest mounts XFS to replay its log, unmounts,
runs `xfs_repair -n`, remounts and runs `git fsck --full --no-dangling` on retained
bare stores; and that only a successful initialized guest clears the flag.
Inject an XFS-check failure and an unreachable guest and verify the flag stays
set and no Run reaches the runner. A clean start must run none of these checks.
The image needs `xfs_repair` and Git. No tenant filesystem is ever mounted by the
host kernel, and recovery never uses `xfs_repair -L` or deletes disk data.

## Qualification-only broker transport

An optional `fake_broker_url` in the host JSON config redirects broker HTTP and
JWKS fetching to one loopback HTTP origin (`http://127.0.0.1:PORT` or
`http://[::1]:PORT`). It is not read from the environment. The config path and
its ancestors must be root-owned, non-symlink and not writable by group/other;
`listen` must also be loopback. Startup emits a warning naming the override.
Redirects are refused, and token `iss` verification remains pinned to the normal
broker identity used by the fake broker's fixtures. Without this field the
production broker/JWKS URLs are unchanged. Never enable this field for a
customer-serving host. `script/tenant-image/qualify.py` uses it only with an
isolated journal and disposable tenant for operator qualification.

Guest initialization, quota I/O and job-relay failures are `lease_refused` with
`reason: "internal_failure"`, not retryable capacity. Host logs retain the exact
guest/helper error and stage-specific wake/quota/relay records; clients receive
only the generic typed reason. Actual capacity/host-full/expiry/auth refusals keep
their previous meanings. The hosted client recognizes internal failure both in
the lease prelude and after lease acceptance while awaiting the runner Hello,
and does not request replacement leases for it.
