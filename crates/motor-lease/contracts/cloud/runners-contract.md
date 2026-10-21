# Runners wire contract

The broker is `https://runners.cortexkit.io` (`workers/runners`). It is a control plane only:
ck-motor receives a lease and connects directly to the placed host. Job bytes never pass through
the broker. [runners.md](runners.md) covers architecture, operator commands, deployment and key
rotation. The pinned bytes under `test-vectors/runners/` are part of this contract.

## Credentials and job tokens

Client endpoints require `Authorization: Bearer <token>`, with that exact scheme and one space.
The account service issues this machine worker token with protected header `typ: ck-worker+jwt`,
audience `cortexkit-runners` and exactly the claims `account`, `device`, `iat`, `exp`, `aud`, `jti`.
The broker uses `WorkerJwksVerifier` from `packages/account-auth` and the account JWKS at
`https://account.cortexkit.io/.well-known/jwks.json`. The tenant is `acct:` plus the verified
account byte for byte. Any verified account can call the client routes; device is not an
authorization boundary. A runners token is not an account machine principal.

The **job token** is a different credential, signed by the broker for the placed host. It is a
compact JWS with unpadded base64url segments. The header JSON bytes are exactly:

```json
{"alg":"EdDSA","kid":"<signing key id>","typ":"ck-job+jwt"}
```

Payload keys occur in this order, without whitespace:

| Claim | Value |
| --- | --- |
| `iss` | `https://runners.cortexkit.io` |
| `aud` | Placed host id, equal to `placement.host` |
| `sub` | Tenant `acct:<ULID>` |
| `jti` | `job_id` |
| `iat` | `floor(broker_clock_ms / 1000)` at issue |
| `exp` | `iat + 300` |
| `vcpus`, `mem_mib`, `platform`, `net` | Leased resources and options |
| `placement` | `{"host":"<host id>","vm":"<warm VM id>"}` in that key order, or `vm: null` for cold placement |
| `max_s` | Run limit enforced by the host from job start, not token issue |
| `cnf` | `{"jkt":"<thumbprint>"}` |
| `purpose` | `"run"` from lease; `"attach"` from reattach |

`cnf.jkt` is the unpadded base64url SHA-256 RFC 7638 thumbprint of the UTF-8 JSON
`{"crv":"Ed25519","kty":"OKP","x":"<requester x>"}`. The token is bound to the requester key.

Hosts check expiration at connect only. They reject any other `alg` or `typ`, unknown `kid`, any
`crit` header, wrong `iss` or `aud`, any purpose other than `run` or `attach`, and extra or missing
claims. Host executions are keyed by `jti`: a `run` token starts an execution only if none exists
for that job; later tokens reconnect without starting another execution or restarting `max_s`.
An `attach` token never starts an execution. A `run` token for an ended execution is refused. An
`attach` token for an ended execution is accepted while the host retains the job's terminal
record: the host replays the retained output and terminal records and reports no further usage.
These are host obligations, not execution logic implemented in the broker.

`GET /.well-known/jwks.json` is unauthenticated and returns 200 `{"keys":[…]}` with
`Cache-Control: public, max-age=300`. Each key is
`{kty:"OKP",crv:"Ed25519",x,kid,alg:"EdDSA",use:"sig"}`. The current key is first, followed by next
and previous when configured. Each `x` is the unpadded base64url 32-byte public key derived from
the corresponding Ed25519 seed. There is never a `d` member. Only current signs; previous remains
usable for verification while configured. Machine worker tokens use the account JWKS, never
this endpoint.

Hosts refetch this set every 10 minutes, and immediately when a job token names a `kid` they don't
hold, at most once a minute. A failed fetch keeps the last good set. Rotation publishes the next
key at least 7 days before it starts signing, so a host that refetches on schedule never sees an
unknown `kid` in normal operation.

## Common wire rules

Routing comes first. A wrong method or unknown path is 404 `{"error":"not_found"}` before any
authentication or body check, with no writes. Unexpected failures return 500
`{"error":"internal"}`, never backend text or a stack.

Bodies are UTF-8 JSON. A POST requires media type `application/json`; parameters such as
`charset=utf-8` are accepted. Missing or different Content-Type, or more than 65536 raw body
bytes, is 400 `bad_request` at the route's specified check position. GET routes have no body and
no Content-Type check. Objects at every nesting level have exactly the documented keys: unknown,
missing or duplicate keys are invalid. Integers are safe JSON numbers without fraction or
exponent. Body field order need not match the canonical order.

| Field | Grammar |
| --- | --- |
| `job_id` | Lowercase UUIDv7: `^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$` |
| `workspace_key_hash`, `repo_hash` | `^[A-Za-z0-9_-]{43}$`: opaque to the broker, which checks only this shape and compares values for equality (see below) |
| `platform`, `host_id`, warm `vm`, non-null `avoid_host` | `^[A-Za-z0-9._-]{1,64}$` |
| Account ULID | `^[0-7][0-9A-HJKMNP-TV-Z]{25}$` |
| Warm `tenant` | `acct:` followed by an account ULID |
| `requester_jwk` | Exactly `{kty:"OKP",crv:"Ed25519",x}`, where `x` is unpadded base64url encoding 32 bytes |
| `endpoint_spki_sha256` | `^[A-Za-z0-9_-]{43}$`: unpadded base64url SHA-256 of the host certificate's DER SubjectPublicKeyInfo (32 bytes) |

The endpoint is `host:port` (bracketed for IPv6), not a URL with a scheme or path. The endpoint
and its SPKI hash come byte-exact from the host registry. Clients use
`endpoint_spki_sha256` to pin the host certificate's public key on the direct TLS connection; it
is not a certificate fingerprint or the host's report-signing Ed25519 key.

`platform` names the operating system and CPU architecture together, for example `linux-x64` or
`linux-arm64`. The broker gives it no other meaning: a lease is placed only on a host whose
registered platform is byte-for-byte the same string. Clients send the platform of the job's
toolchain, and hosts register their own, so hosts of a new architecture just add a new string.

Clients derive `workspace_key_hash` and `repo_hash` as unpadded base64url HMAC-SHA256 under a
per-machine secret salt, over `ck-ws-v1\n<workspace path>` and `ck-repo-v1\n<repository path>`
respectively. Because the salt never leaves the machine, the broker and hosts can match the same
workspace across leases for warm placement without learning or guessing its path.
`test-vectors/runners/lease.json` records a TEST-ONLY salt and inputs that reproduce its values.

Canonical JSON for hashing uses the body keys in each endpoint's listed order and no whitespace;
`requester_jwk`, where present, uses `kty`, `crv`, `x`. The lease hash omits `requester_jwk`.
The usage hash covers the full canonical usage body. A host signature instead hashes the raw
request bytes, not canonical JSON.

### Error catalog

Every error body is exactly `{"error":"<code>"}`, with no other member, except `no_host_fits`,
which is exactly `{"error":"no_host_fits","max_vcpus":<integer>}` in that member order.

| Status | Code |
| --- | --- |
| 400 | `bad_request`, `platform_unsupported` |
| 401 | `unauthorized` (client routes), `host_auth_failed` (host routes) |
| 402 | `budget_exhausted` |
| 403 | `shadow_not_allowed` |
| 404 | `not_found`, `job_unknown` |
| 409 | `job_conflict`, `job_admitted`, `job_released`, `job_finished`, `job_settled`, `usage_conflict`, `sandbox_lost` |
| 422 | `no_host_fits` (lease only; never carries `Retry-After`) |
| 500 | `internal` |
| 503 | `no_capacity`, `unavailable`; both include `Retry-After`, 5 seconds unless a Boat refusal or the tenant's Boat start limit names a longer wait |

Clients treat `usage_conflict`, `job_settled` and `job_unknown` as final for that report, not
retryable transport failures. `sandbox_lost` is final for that job: its Boat sandbox stopped or
failed under the lease, and ck-motor must start a new `job_id`. `no_host_fits` is final for that
lease body: retrying it never succeeds while the registry is unchanged, so the client routes the
job elsewhere or asks for at most `max_vcpus`. Host retries after a consumed counter must use a new counter.

## Client endpoints

After routing, client authentication precedes Content-Type, size, JSON parsing and all state
access. A missing or invalid token returns 401 `unauthorized` even with no Content-Type, an empty
body or an oversized body; no totals or rows change. Once authenticated, grammar or bound
failures, including a path `job_id`, are 400 `bad_request` without writes.

### POST /v1/leases

The exact body fields, in canonical order, are:

```text
{job_id, vcpus, platform, net, workspace_key_hash, repo_hash,
 placement_hint, avoid_host, timeout_s, requester_jwk}
```

plus optional boolean members `shadow` and `allow_spill`, described under "Shadow leases" and
"Per-lease spill policy" below. No other members are accepted.

The canonical lease hash uses this exact member order:

```text
{ job_id, vcpus, platform, net, workspace_key_hash, repo_hash, placement_hint, avoid_host, timeout_s, allow_spill?, shadow? }
```

It omits `requester_jwk`. `allow_spill` goes immediately after `timeout_s` and before the
optional `shadow`; it is included only when the request has the key, with its exact boolean
value (both `true` and `false` are hashed when present). `shadow` stays last and is included
only when true. `test-vectors/runners/no-spill-lease.json` records `canonical_lease_bytes`,
the exact canonical JSON string hashed, for clients to byte-compare. Omitting
`allow_spill` leaves every existing request's hash byte-for-byte unchanged; explicitly sending
`true` or `false` changes the hash. A re-request must repeat both its presence and value or
receive 409 `job_conflict` wherever a differing lease hash conflicts. In particular, absent and
`true` have identical spill behavior but different request identities.

`vcpus` is 1–64 and `timeout_s` 10–7200. `net` is boolean. There is no client `mem_mib` field:
the broker derives memory as `max(16384, 3584 × vcpus)` MiB for placement and the token.
`placement_hint` is JSON `null` or exactly `{host, vm}`, both strings following the common name
grammar, except that `host` may also carry the `boat:` prefix of a Boat placement, so a client can
send back the placement it was last given. A matching selectable warm placement is preferred; an
unusable hint, including any `boat:` hint, falls back to normal placement. Other strings and the requester key follow the grammar table under "Common wire rules" above.

`avoid_host` is required: JSON `null`, or a host id string matching
`^[A-Za-z0-9._-]{1,64}$` (no `boat:` prefix; a Boat sandbox is not a host). A body without the
key, or with any other value, is 400 `bad_request`. The named host is not selectable for this
lease, even when `placement_hint` names it; placement proceeds through the other hosts and then
Boat spill exactly as if that host had failed selection. ck-motor sends the host it is spilling
away from, so a job leaving a busy machine is never placed back on it. Because the host stays
registered, a platform served only by the avoided host answers `no_capacity` (or spills), not
`platform_unsupported`. A value naming no registered host, and `null`, change nothing. The lease
hash covers `avoid_host`, so a re-request must repeat it exactly; a different value is 409
`job_conflict`.

After authentication and schema validation, checks proceed in this order:

1. Read the account's allowance, then claim `job_id` in D1. A claim belonging to another tenant is
   404 `job_unknown` and reveals none of that job's data.
2. Check for an existing `TenantDO` row. A release marker takes precedence and gives 409
   `job_released`, even after settlement. Otherwise a settled job is 409 `job_finished`; a reserved
   job ever reported `admitted` or `started` is 409 `job_admitted`. Otherwise an identical lease hash
   returns 201 on the same reservation, with a fresh `run` token for this requester's key. A
   different hash is 409 `job_conflict`. These replies do not reserve again.
3. Ask for placement. If no active host has the requested platform, return 400
   `platform_unsupported`. If active hosts have it but none is selectable, return 503
   `no_capacity`, unless no candidate host could ever fit the lease (below), which is 422
   `no_host_fits`. Before returning a refusal, repeat the existing-job check so a concurrent
   successful lease wins over that refusal. When `allow_spill` is not `false`, Boat spill is
   enabled and the platform equals `BOAT_PLATFORM`, either 400 or 503 refusal is replaced by the
   spill path in [Boat placements](#boat-placements), and so is `no_host_fits` when a Boat size
   fits the lease.

   **Never fits (`no_host_fits`).** The candidates are every `status = 'active'` host of the
   lease's platform except the one `avoid_host` names. Health, heartbeat age, generation and free
   capacity are ignored: they are temporary, so a host that could fit when idle and healthy
   keeps the answer at `no_capacity`. A candidate can fit when its registry per-job ceilings are
   both recorded and `max_job_vcpus ≥ vcpus` and `max_job_mem_mib ≥ max(16384, 3584 × vcpus)`.
   If any candidate's ceiling is unrecorded (NULL), the broker never concludes "never fits" and
   answers exactly as without this rule. With at least one candidate, all ceilings recorded and
   none able to fit, the reply is **422** `{"error":"no_host_fits","max_vcpus":N}` with no
   `Retry-After`. Without any candidate (no active host has the platform, or only the avoided host
   does) the codes above apply unchanged. When the lease allows spill, Boat is available for its
   platform (enabled, configured, with a current image) and a Boat size fits, the lease spills
   instead. `N` is the largest vCPU count that could be placed: for each candidate, the largest
   `v` whose `v` and `max(16384, 3584 × v)` both fit its ceilings (0 if none), maximized over
   candidates; when the lease allows spill and Boat is available for its platform, also the
   largest Boat size's `vcpus`. A `no_host_fits` refusal makes no Boat call and leaves no
   `TenantDO` job row, reservation, provisioning hold, pool claim or spill record, so a
   re-request of the same body gets the same answer. `test-vectors/runners/no-host-fits-lease.json`
   pins one such request and its exact reply bytes.
4. Compute the budget candidate from current totals. For finite allowances,
   `remaining = max(0, allowance − settled_total − reserved_total)` and
   `max_s = min(timeout_s, floor(remaining / (vcpus × 1000)))`; unlimited uses `timeout_s`.
   If below 10, repeat the existing-job check before returning 402 `budget_exhausted`.
5. Sign outside the transaction, then in one synchronous transaction repeat the existing-job
   check and recompute the limit. Commit only if it still matches the signed candidate, inserting
   the unique job row and reserving `vcpus × max_s × 1000`. A changed limit below 10 is 402;
   otherwise retry signing. Three mismatches return 503 `unavailable` without a reservation.
   A concurrent existing-row hit is not a mismatch attempt and reserves nothing.

Thus reservations cannot exceed remaining budget. Each stored `max_s` equals its token's limit;
separately, each stored reservation equals `vcpus × max_s × 1000`. For example, allowance 25000
with no totals, 2 vCPUs and timeout 100 gives `max_s: 12`, reservation 24000; 20000 gives 10;
19999 refuses. Unlimited, 64 vCPUs and 7200 seconds reserves 460800000.

Placement and budget refusals leave no `TenantDO` job row and unchanged totals, but retain the D1
claim. The same tenant can retry after capacity returns or allowance increases; another tenant
still gets 404. Existing-job refusals never mutate its row or totals.

Success is **201** with exactly these fields (the same shape is used by reattach):

```text
{lease_id, job_id, placement:{host,vm}, endpoint, endpoint_spki_sha256,
 job_token, token_expires_at_ms, max_s, reserved_vcpu_ms}
```

`lease_id` is a lowercase UUID. `vm` is a warm VM string or `null` for cold placement. `endpoint`
and `endpoint_spki_sha256` are the placed host's registry values. `job_token` has purpose `run`;
`token_expires_at_ms` is its `exp × 1000`.

A re-request may change only `requester_jwk`, retaining the same lease id, placement, endpoint,
SPKI hash, run limit and reservation. The new token has a fresh issue time and 300-second life.
If a token is expired before connecting, re-request before admission or use reattach; never
connect with a token whose `token_expires_at_ms` is not after the client's current time. Neither
refresh path extends the original host-lost deadline or starts another execution.

Placement is not a capacity lock. On a full host, a new placement needs a new `job_id`; retrying
the same lease keeps the same placement. The host releases the unused first reservation with a
zero-duration usage report, or it remains until the host-lost alarm settles proved elapsed time.

### POST /v1/leases/{job_id}/reattach

The body is exactly `{requester_jwk}`. Success is **200** with exactly the lease reply fields:

```text
{lease_id, job_id, placement:{host,vm}, endpoint, endpoint_spki_sha256,
 job_token, token_expires_at_ms, max_s, reserved_vcpu_ms}
```

Lease id, placement, endpoint, SPKI hash, run limit and reservation remain unchanged. The new
`attach` token is bound to the supplied key and has its own `iat` and `exp = iat + 300`. It reserves
and settles nothing and does not move the original deadline. Earlier tokens are not revoked;
they expire at their own `exp`. Unknown, claim-only or another tenant's jobs return 404
`job_unknown`. For a known job the broker then decides, in this order:

1. A release marker is 409 `job_released`, even after settlement.
2. A job settled with reason `lost` (a lost Boat sandbox) is 409 `sandbox_lost`.
3. A settled registered-host job (any provider but Boat) whose settlement came from its host's
   usage report (reason `reported`), with a recorded settlement time `settled_at_ms` such that
   `settled_at_ms + 86400000 > now` (86400 s), returns 200 with an `attach` token and exactly the
   reply an unsettled host job gets: the same lease id, placement, endpoint, SPKI hash, `max_s`
   and `reserved_vcpu_ms`. The host keeps such a job's terminal record and output for 24 hours
   and replays them to an `attach` token, so a client that lost the end of the output can recover
   it. This reattach reserves and settles nothing, changes no totals, writes no usage and changes
   no job field. The window is registered-host only because a Boat sandbox is reused for another
   job and its SSH keys rewritten, so an attach could reach a sandbox now serving someone else.
4. Every other settled job is 409 `job_finished`: Boat jobs, jobs settled as `host_lost`,
   `released` or `deadline` (or `finished`), and any settled job whose settlement is 86400 s old or
   older or has no recorded settlement time (jobs settled before the time was recorded).

The same rule is checked again after signing, so a job released or settled for any other reason
while its token was being signed is refused with the code above, and a job settled by its host's
usage report in that time still gets the token. Lease re-request and release keep answering a
settled job with 409 `job_finished` (or `job_released`). `test-vectors/runners/settled-reattach.json`
pins a reattach for a usage-settled host job with the tenant totals before and after it.

### POST /v1/leases/{job_id}/release

The body is exactly `{}` with JSON Content-Type. Authentication still precedes path and body
validation. Unknown, claim-only or another tenant's jobs return 404 `job_unknown`; a settled job
returns 409 `job_finished`; an admitted job returns 409 `job_admitted`, in that order.

Otherwise success is **202** with exactly `{job_id, release_at_ms}`. The first release records
`release_at_ms = latest_run_token_expires_at_ms + 60000`. Repeated releases before settlement
return the same time and change no totals. Release is delayed cancellation, not immediate
settlement: outstanding run tokens must expire first. A release marker blocks lease re-request
and reattach with 409 `job_released`.

An `admitted` or `started` heartbeat that wins before settlement clears the marker and keeps
the reservation; a `reserved` heartbeat preserves it. The alarm settles an unadmitted job for
zero charge once release is due, releasing its entire reservation. The marker remains, so
lease re-request and reattach continue returning `job_released`; another release returns
`job_finished`. Usage after release settlement returns `job_settled`.

**Known gap: a released job can run unbilled.** Every executed job should be billed, but this
contract doesn't guarantee it for a released job that a host admits anyway. Billing depends on
that host reporting `admitted` or `started` before `release_at_ms`. If those heartbeats don't
arrive in time, for example because the host can't reach the broker, the alarm settles the job at
zero and its later usage report gets `job_settled`. Hosts narrow the window by refusing to admit a
job whose run token has expired, which leaves at least 60 seconds for the heartbeat.

### POST /v1/leases/{job_id}/finish

Boat placements only (nothing inside a Boat sandbox signs to the broker, so ck-motor reports the
end). Authentication and release's D1 claim check run first: an unknown, claim-only or other
tenant's job is 404 `job_unknown`. The body is exactly `{outcome, run_ms}` with JSON Content-Type;
`outcome` is `exit`, `signal`, `timeout`, `cancelled` or `lost`, and `run_ms` a non-negative safe
integer of at most `max_s × 1000`. Anything else is 400 `bad_request`. Then, in order:

1. A job not placed on a Boat sandbox: 404 `job_unknown`.
2. A job settled as `lost`: 409 `sandbox_lost`.
3. A job settled by an earlier finish: the identical canonical body (`{outcome, run_ms}` in that
   order) returns the original 200 reply; any other body gives 409 `usage_conflict`. Settled any
   other way: 409 `job_settled`.
4. Otherwise settle on the broker clock (see [Boat placements](#boat-placements)) with reason
   `finished`, record `outcome` and `run_ms` as untrusted facts, and rewrite the key file empty.

Success is **200** with exactly `{job_id, settled_vcpu_ms, released_vcpu_ms}`. `run_ms` never
affects the amount.

### GET /v1/tenant

Success is **200** with exactly:

```text
{tenant_id, allowance, reserved_vcpu_ms, settled_vcpu_ms,
 running_jobs:[{job_id, host, vm, vcpus, max_s, reserved_vcpu_ms, last_elapsed_ms}]}
```

`allowance` is the JSON string `"unlimited"` or a JSON number, never a numeric string. A missing
allowance row means 0. `running_jobs` contains only reserved jobs, ordered by `job_id`. An account
with no state reads totals 0 and `running_jobs: []`. The route writes no D1 row or Durable Object
storage.

## Host authentication

Hosts are operator-enrolled with a raw 32-byte Ed25519 public key encoded as 64 lowercase hex
characters. Retired hosts still authenticate and settle existing jobs; they are excluded only
from new placement.

HTTP header names are case-insensitive. Values are exact:

| Header | Value |
| --- | --- |
| `CK-Host` | Must equal path `host_id` |
| `CK-Timestamp` | Unix milliseconds, `^[1-9][0-9]*$`, safe integer |
| `CK-Counter` | Positive safe integer, `^[1-9][0-9]*$`, strictly above the last accepted value |
| `CK-Signature` | 64-byte Ed25519 signature as 128 lowercase hex characters |

**Host startup rule:** seed the counter on start as `max(persisted + 1, unix_ms × 1000)`.
Counters may have gaps. Persist and advance them across requests; a retry after counter acceptance
must use a new value, even when the original request failed later checks.

Sign exactly the UTF-8 string below, with literal newlines and **no trailing newline**:

```text
ck-host-v1
POST
<pathname>
<lowercase hex sha256 of raw body bytes>
<CK-Timestamp>
<CK-Counter>
```

The pathname is as received, not percent-decoded. Timestamp and counter are the raw header
values. Signature verification precedes JSON parsing.

After routing, checks run in this order:

1. Invalid path-host grammar, any query string, missing/wrong Content-Type or an oversized body:
   400 `bad_request`, with no `HostDO` storage or counter write.
2. Malformed headers, a `CK-Host` mismatch, an unregistered host or a bad signature against its
   registered key: 401 `host_auth_failed`, with no counter write. Signing a request for host B
   with host A's key changes nothing on B. Retirement is not an authentication failure.
3. `HostDO` accepts only a counter strictly above its last accepted counter. Replay or a
   non-increasing value is 401 `host_auth_failed` without change. The initial counter is 0;
   stored counters belong to the registry generation. Retirement followed by re-registration
   increments generation, so the new generation starts from 0 and can accept counter 1.
   A passing counter is persisted before all remaining checks.
4. Timestamp skew greater than 60000 ms from the broker is 401 `host_auth_failed`; exactly
   60000 ms is accepted. A skew refusal consumes the counter but applies no body effects.
5. Schema and bounds are checked: invalid JSON or fields are 400 `bad_request`, with the
   counter consumed.
6. A failed downstream `TenantDO` write is 503 `unavailable`; retry with a new counter.

All authentication failures are exactly `{"error":"host_auth_failed"}`. Post-counter 4xx
outcomes never roll the counter back.

### POST /v1/hosts/{host_id}/heartbeat

The exact body is:

```text
{free_vcpus, free_mem_mib, queued, warm:[{tenant, workspace_key_hash, vm}],
 running:[{job_id, state, elapsed_ms}], healthy}
```

Integers are non-negative safe integers. `healthy` is boolean. `warm` and `running` each have at
most 64 entries. `state` is exactly `reserved`, `admitted` or `started`. The heartbeat replaces
the host capacity snapshot, timestamped at broker receive time. `queued` is stored but does not
affect placement.

Running entries are processed in order. An unknown, claim-only or other-host job is listed in
`ignored` without failing siblings or writing `TenantDO` storage. A settled job placed on this
host has no effect. A reserved job on this host updates
`last_elapsed_ms = max(last_elapsed_ms, min(elapsed_ms, max_s × 1000))`. States `admitted` and
`started` permanently mark it admitted; later `reserved` does not clear that flag. This prevents
lease re-request, but not reattach.

Success is **200** with exactly `{"ignored":[<job_id>…]}`, strings in entry order and no other
member. A valid heartbeat from a retired host still updates elapsed time and snapshots.

### POST /v1/hosts/{host_id}/usage

The exact body, also its canonical hash order, is:

```text
{job_id, vcpus, started_at_ms, ended_at_ms, mem_peak_mib, egress_bytes, outcome}
```

`vcpus` is 1–64. Times, `mem_peak_mib` and `egress_bytes` are non-negative safe integers;
`ended_at_ms ≥ started_at_ms`. `outcome` is exactly `exit`, `signal`, `timeout`, `cancelled` or
`lost`. Outcome and memory/egress measurements are audit-only. The host must cap its own report
at `max_s` (`ended_at_ms − started_at_ms ≤ max_s × 1000`); independently the broker caps
settlement at the reservation even if the host over-reports.

After authentication and schema checks, precedence is:

1. Unknown, claim-only or other-host job: 404 `job_unknown`. The signing host's counter is
   consumed; the job's host and totals are unchanged.
2. Settled job: after a host-lost or release settlement, 409 `job_settled`. After a reported
   settlement, an identical canonical usage hash returns the original 200 reply; any other body gives 409
   `usage_conflict`. These replies do not settle again.
3. Reserved job whose `vcpus` differs from the lease: 400 `bad_request`; it stays reserved.
4. Settle `S = min(leased_vcpus × (ended_at_ms − started_at_ms), reserved_vcpu_ms)` and release
   `reserved_vcpu_ms − S`. Zero duration settles 0 and releases the full reservation.

Success is **200** with exactly:

```text
{job_id, settled_vcpu_ms, released_vcpu_ms}
```

A repeated authenticated report needs a new `CK-Counter` even when its canonical body is
identical: authentication consumes the counter before settlement idempotency is checked.

## Placement, deadlines and metering

Selectable hosts are active, platform-matching, healthy, no more than 60000 ms since heartbeat,
have a snapshot for their current registry generation, and have sufficient free vCPUs and memory.
Prefer the placement hint if it names a selectable warm entry matching tenant and
`workspace_key_hash`. Otherwise prefer any matching warm entry, then bytewise host id and VM id.
Without a warm match, choose the lowest selectable host id with `vm: null`.
`repo_hash`, `net` and `queued` do not affect placement.
Capacity snapshots do not lock hosts or VMs.

The original lease fixes the host-lost deadline at
`original_lease_iat × 1000 + max_s × 1000 + 600000`. Neither reattach nor lease re-request moves
it. When due, the alarm settles `min(vcpus × last_elapsed_ms, reserved_vcpu_ms)`, zero without
proved elapsed time, and releases the rest. Usage and alarm settlement serialize; the first
transition wins. Usage after host-lost settlement is 409 `job_settled` and changes no totals.
An unadmitted release can instead settle for zero at its release deadline, as described above.

Each nonzero settlement adds its settled vCPU-ms exactly once to the UTC-hour `runners` /
`vcpu_ms` aggregate. Zero settlements add nothing. Audit retries and repeated reports do not
count again. These hourly facts go to account's `RunnersUsageIngest`; the account ledger is not
the broker's reservation authority. Allowances are lifetime vCPU-ms; a missing row means 0, and
`unlimited` removes the budget gate. Prices and invoices are outside this contract.

## Boat placements

When no registered host fits a lease, the broker can spill it to a Boat (boat.dev) sandbox that it
drives through Boat's API. Spill is off unless `BOAT_ENABLED` is `"true"` and every Boat setting
is configured; with it off no lease spills, while sandboxes that already exist are still stopped
and cleaned up. Each tenant has a pool of at most `BOAT_MAX_SANDBOXES` sandboxes, and **a sandbox
serves at most one live lease**. Placement hints never select a sandbox.

**Per-lease spill policy.** The optional `allow_spill` member is strictly a JSON boolean; any
other value is 400 `bad_request`. `false` requires a registered host: when placement refuses,
the broker returns that refusal unchanged (including its status, body and Retry-After), never
claims or creates a Boat sandbox, makes no Boat calls and leaves no spill record for that lease.
For example, a full registered fleet returns 503 `no_capacity` with Retry-After `5`, and a job
larger than every registered host's per-job ceiling returns 422 `no_host_fits` whose
`max_vcpus` counts registered hosts only.
`true` or omission preserves ordinary spill behavior. This is per lease, not per tenant, so
callers sharing a fleet account can choose independently. The field is optional during the
cutover; once ck-motor always sends it, the broker will make it required in a later change.
It does not override the Boat enablement, platform, size or budget gates. `shadow: true` with
`allow_spill: false` is contradictory and returns 400 `bad_request` during schema validation,
before shadow permission or any job claim is checked.

**Eligibility.** The lease's platform equals `BOAT_PLATFORM` (`linux-x64`), and `BOAT_SIZES`
(the usable `{type, vcpus, mem_mib}` measured inside each Boat size) has a size with
`vcpus ≥ job.vcpus`; the smallest such size is used. Memory does not decide eligibility. No
fitting size is 503 `no_capacity`, or 422 `no_host_fits` when no registered host could ever fit
the lease either (see step 3 of the lease order).

**Order.** The budget candidate is computed first, exactly as for hosts; below 10 seconds the
lease is 402 `budget_exhausted` and no Boat call is made. While the shared Boat breaker is open
(after a Boat 429 or 503), the lease is 503 `no_capacity` at once with that wait as Retry-After.
Then, in one transaction that computes the budget candidate again (402 `budget_exhausted` below
10 seconds), a provisioning record keyed by `job_id` claims a pool slot: a free ready sandbox,
else an archived one to resume, else one being stopped (waited for, then resumed), else a new
sandbox under the cap. Every slot but a free ready one needs a start (a create or a resume), and
a tenant gets at most its hourly start limit in any hour, resumes included: the account's own
limit when the operator set one (1 to 1000), else `BOAT_MAX_STARTS_PER_HOUR` (6). Past that the
lease is 503 `no_capacity` with Retry-After the wait until a start leaves the hour. No slot is
503 `no_capacity`.

**Provisioning hold.** The claiming transaction reserves a provisioning hold of
`min(vcpus × 60000, vcpus × max_s × 1000)` vCPU-ms, the minimum a booked Boat job is charged; it
appears in the tenant's `reserved_vcpu_ms`. Booking turns the hold into the job's reservation. A
record that ends without a booking (it expired, Boat refused the start, or the budget was gone)
**charges the hold** if a Boat create or resume was sent for it: it moves to `settled_vcpu_ms`,
reason `lost`, on the broker clock, and like every Boat settlement is never counted in the usage
aggregate. This includes a resume whose reply was lost and whose retry Boat then refused: the first
attempt may have started the machine. A record that started nothing releases its hold.

**Provisioning reply.** While the claimed sandbox starts (until Boat reports it `ready` and
`hydrated`, then through setup), the answer is **202** with exactly
`{"status":"provisioning","retry_after_ms":5000}`. A re-request with the same `job_id` and lease
hash joins the record and answers from broker state; a different body is 409 `job_conflict`. A
record lives 180 seconds; after that, or after Boat refuses to start the sandbox (answered once as
503 `no_capacity` with Boat's wait), the next re-request starts over. A re-request after the record
expired is a new lease in every respect: registered hosts are tried first, so its answer can be an
ordinary host reply (201, no `provider`) for the same `job_id`, and Boat is used only if no host
fits.

**Booking.** The reservation is booked in the request that returns the lease, in one transaction
that re-checks the budget (counting the record's own hold as available) and that the record is
the same one the request read (same generation and lease hash, before its deadline on the
broker's current clock, a sandbox of its size), and only if Boat's auto-stop for the sandbox falls
after the lease's deadline. Anything else answers 202, and the re-request decides afresh. A
booked Boat job is **admitted at booking**: release answers 409 `job_admitted`. The key file is
then written and read back; if that fails the job stays booked and re-requests answer 202 while
the broker retries, and after 3 failures the job settles as `lost`.

**Reply.** **201** with exactly these fields, in this order (reattach uses the same shape with
200 and an `attach` token):

```text
{provider:"boat", lease_id, job_id, placement:{host:"boat:<sandbox_id>", vm:"<sandbox_id>"},
 ssh:{endpoint, host_key, user}, runner_port, job_token, token_expires_at_ms, max_s, reserved_vcpu_ms}
```

Host replies are unchanged and carry no `provider`. The job token's `aud` and `placement.host`
are `boat:<sandbox_id>`, with Boat's sandbox id verbatim (grammar
`^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$` from Boat's OpenAPI file); the colon can never
match a registered host id. The token's `mem_mib` is the chosen size's usable `mem_mib`, the
sandbox's whole usable memory, not the fleet formula. `runner_port` is 9443.

**SSH transport.** ck-motor connects to `ssh.endpoint` (`host:port`, an IPv4 or bracketed IPv6
host) as `ssh.user`, pins `ssh.host_key` (exactly `ssh-ed25519 <base64>`, no options or comment),
and opens one local forward to `127.0.0.1:<runner_port>` with `-N`, then runs the normal token and
admission challenge over it. The endpoint and host key come from Boat's `POST /sshkey` reply,
fetched once per sandbox incarnation (they change on every resume), and every re-request and
reattach returns the sandbox's current pair. The sandbox's `~/.ssh/authorized_keys` holds exactly
one line, for the lease's requester Ed25519 key (the one `cnf.jkt` names):

```text
restrict,port-forwarding,permitopen="127.0.0.1:9443",command="/bin/false" ssh-ed25519 <base64(string "ssh-ed25519" || string pk)>
```

SSH strings are a big-endian uint32 length followed by the bytes. A re-request or reattach with a
different requester key rewrites the line to that key before any token is returned. Finish,
settlement and a deadline rewrite the file empty. The file is hygiene, not revocation: the tenant
has root in its own sandbox, and established sessions survive a rewrite.

**Settlement (broker clock only).** A finished or lost Boat job settles
`min(max(vcpus × (settled_at_ms − booked_at_ms), vcpus × 60000), reserved_vcpu_ms)`, where
`settled_at_ms` is when the broker received the finish or observed the loss. At the host-lost
deadline an unfinished Boat job settles its full reservation with reason `deadline`. If its
sandbox stops or fails under the lease (Boat's auto-stop, a Boat fault, a guest crash) or the key
file cannot be written, the job settles with reason `lost`, and lease re-request, reattach and
finish answer 409 `sandbox_lost`. Boat settlements update the tenant's budget totals but are
**never counted** in the `runners` / `vcpu_ms` usage aggregate; their facts (and Boat's own
machine-time meter per run window) are recorded in D1 only, until a billing rule exists.

**Shadow leases.** A shadow lease runs a copy of a job on Boat purely to compare its result with
the same job on the build server; its caller is served elsewhere. The lease body may carry
`shadow: true`. Absent and `false` mean an ordinary lease and canonicalize identically: the
canonical lease hash omits the member then, so ordinary leases hash exactly as before, and appends
`"shadow":true` as the last member when it is true (after `allow_spill`, if present).
A re-request must repeat it. Any other value is 400 `bad_request`.

- Only accounts with the operator-set shadow permission in D1 `allowances` may send it; any other
  account gets 403 `shadow_not_allowed`, checked right after schema validation.
- A shadow is placed on Boat only, never on a registered host, and only while `BOAT_SHADOW_ENABLED`
  is `"true"`, independently of `BOAT_ENABLED` (which governs ordinary spill). With shadows off,
  Boat unavailable, or the tenant at `BOAT_MAX_SHADOWS` concurrent shadows (default 2), the answer
  is 503 `no_capacity`; the client skips that shadow and never falls back.
- A shadow counts toward the pool's sandbox cap, the hourly and daily Boat start limits and the
  daily start count, like any Boat lease.
- A shadow is never charged to the tenant's allowance and never counted in the account usage
  ledger. Its job is still recorded in D1 `provider_jobs` with a shadow flag and Boat's own
  machine-time meter, so the cost of shadow-running stays visible.
- Re-request, reattach, release, finish and settlement otherwise behave exactly as for an ordinary
  Boat lease.

**Image obligations** (the tenant sandbox image):

- Tenant sandboxes are created confined (`noEnv`), so the snapshot must not depend on any account
  secret, and is built from a `noEnv` sandbox through the clean image pipeline.
- The broker writes `/etc/motor/tenant/id` after every create or resume, once Boat reports the
  sandbox hydrated, root-owned `0444`, holding the sandbox id as ASCII with no newline: it stages
  the id with Boat's file API at `/tmp/ck-motor-tenant-id`, then runs `sudo install -D -m 0444 -o
  root -g root /tmp/ck-motor-tenant-id /etc/motor/tenant/id && rm -f /tmp/ck-motor-tenant-id &&
  cat /etc/motor/tenant/id` and requires the printed id back. The runner reads it, refuses all
  connections while it is missing, and checks the token's `aud` equals `boat:<id>`.
- The runner listens only on `127.0.0.1:9443`.
- Once the runner listens with its signing keys loaded, the image writes `/run/motor/endpoint-ready`
  holding the sandbox id (the content of `/etc/motor/tenant/id`, a trailing newline allowed). The
  broker reads it with the fixed command `cat /run/motor/endpoint-ready` through Boat's exec API
  after every start, create or resume, and books no lease onto the sandbox until it names this
  sandbox. `/run` is cleared at boot, so the marker always belongs to the current start.
- sshd sets `AllowTcpForwarding local`, `PermitTTY no`, `AllowAgentForwarding no`,
  `X11Forwarding no` and `PermitTunnel no`, and the boot unit empties `authorized_keys` on every
  resume.
- Nothing in the sandbox signs to the broker: no report key, heartbeat or usage report.

## Pinned conformance evidence

`test-vectors/runners/job-tokens.json` pins both token purposes and their header/claim bytes,
the fixed signing seed and its derived public key. `test-vectors/runners/lease.json` pins the
lease request and exact reply, including the lease id and SPKI hash.
`test-vectors/runners/host-signature.json` pins the exact host signature input and signature.
`test-vectors/runners/usage-settlement.json` pins the report and settlement reply.
`test-vectors/runners/settled-reattach.json` pins a reattach for a job its host already settled by
usage report: the 200 `attach` reply with the lease's unchanged fields, and the identical tenant
read before and after it.
`test-vectors/runners/boat-lease.json` pins, with a recorded test size and the recorded test
requester key, the Boat 202 provisioning reply, the booked Boat lease reply, a finish call and
reply, `sandbox_lost` on reattach and finish, and the requester key's OpenSSH encoding and
authorized_keys line.

`test-vectors/runners/no-spill-lease.json` and `test-vectors/runners/no-host-fits-lease.json`
pin the registered-host-only refusals: a full host's 503 `no_capacity`, and the 422
`no_host_fits` for a job above the only host's per-job ceiling.

`workers/runners/test/vectors.test.ts` and `workers/runners/test/boat-vectors.test.ts` compare real
implementation output to those committed bytes. `workers/runners/test/job-token.test.ts`, `workers/runners/test/jwks.test.ts`,
`workers/runners/test/host-auth.test.ts`, `workers/runners/test/usage.test.ts`,
`workers/runners/test/rerequest.test.ts` and `workers/runners/test/lease-concurrency.test.ts`
exercise the corresponding credentials, state transitions and accounting invariants. See
[runners.md](runners.md#tests-and-wire-vectors) for the full explicit test file list.
