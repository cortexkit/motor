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
| `workspace_key_hash` | When enabled by the broker, on registered-host placements only: the lease request's `workspace_key_hash` verbatim (`^[A-Za-z0-9_-]{43}$`); always the last payload key, after `purpose` |

The temporary broker rollout switch `JOB_TOKEN_WORKSPACE_HASH` emits `workspace_key_hash` only
when its value is exactly the string `"true"`. Otherwise the payload and signed token bytes are
unchanged. When enabled, every run and attach token for a newly recorded job placed on a
registered host includes the job's stored hash, including re-request, re-sign, move, replace and
reattach. Tokens for Boat placements (`placement.host` of the form `boat:<sandbox id>`) never carry
it: the broker chooses Boat sandboxes by workspace itself, so the sandbox has no use for the claim,
and Boat token readers keep rejecting it as an unknown claim. While the claim is optional, a token for a job recorded before the broker stored its
hash may omit it, even with the switch on. The broker stores a hash for every newly recorded job.

`cnf.jkt` is the unpadded base64url SHA-256 RFC 7638 thumbprint of the UTF-8 JSON
`{"crv":"Ed25519","kty":"OKP","x":"<requester x>"}`. The token is bound to the requester key.

Hosts check expiration at connect only. They reject any other `alg` or `typ`, unknown `kid`, any
`crit` header, wrong `iss` or `aud`, any purpose other than `run` or `attach`, and extra or missing
claims, except that hosts must accept `workspace_key_hash` as optional during rollout and validate
it against `^[A-Za-z0-9_-]{43}$` when present. Hosts may require it only after the operator announces
the switch is on everywhere and confirms no hashless legacy job is still unsettled or inside its
reattach window. Such jobs cannot be created anymore and age out within the 6-hour run limit plus
the 24-hour settled-reattach window. Host executions are keyed by `jti`: a `run` token starts an
execution only if none exists for that job; later tokens reconnect without starting another
execution or restarting `max_s`.
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
bytes, is 400 `bad_request` at the route's specified check position. Metrics instead has a 16384-byte
limit and returns 413 above it. GET routes have no body and
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
which is exactly `{"error":"no_host_fits","max_vcpus":<integer>}` in that member order, and
`bad_request`, which may also carry `field` as described below.

**The `bad_request` field.** A 400 `bad_request` raised while validating a request body carries an
optional string member `field`, after `error`, naming what failed:
`{"error":"bad_request","field":"timeout_s"}`. Its value is a member path from the broker's own
schema or the fixed value `body`, and never echoes client-supplied names. It is derived as follows:

- A documented member that is missing, of the wrong type, out of range or failing its grammar is
  named by its path, with dots for nested members: `timeout_s`, `placement_hint.host`,
  `requester_jwk.x`. A member of an array element is named without an index: `warm.vm`,
  `running.state`.
- A member the schema doesn't define, at any nesting level, is reported as `body`. The unexpected
  member's own name is never returned.
- A body that isn't JSON or isn't an object is `body`. So is a body the strict JSON rules above
  refuse before any member is identified: invalid UTF-8, a duplicate key, a number with a fraction
  or exponent or outside the safe-integer range, or more than 65536 bytes.
- A rule between members names the member that makes the body invalid: a usage
  `ended_at_ms` before its `started_at_ms` names `ended_at_ms`. A checked value that doesn't match
  the job names that value's member: a finish `run_ms` above `max_s × 1000` names `run_ms`, and a
  usage `vcpus` that differs from the lease names `vcpus`.

When a body has several faults, `field` names one of them; which one is not part of the contract.
A 400 `bad_request` that is not about the body has no `field`: a missing or different
Content-Type, an invalid path `job_id` or path host, and a query string on a host route. `field` is
diagnostic: the broker doesn't log it, and it never changes what the refusal means. Clients must
accept a 400 `bad_request` without `field`. `test-vectors/runners/bad-request.json` records exact
request and reply bytes.

| Status | Code |
| --- | --- |
| 400 | `bad_request`, `platform_unsupported` |
| 401 | `unauthorized` (client routes), `host_auth_failed` (host routes) |
| 402 | `budget_exhausted` |
| 403 | `account_closed` (lease only; never carries `Retry-After`) |
| 404 | `not_found`, `job_unknown` |
| 409 | `job_conflict`, `job_admitted`, `job_released`, `job_finished`, `job_settled`, `usage_conflict`, `sandbox_lost` |
| 422 | `no_host_fits` (lease only; never carries `Retry-After`) |
| 500 | `internal` |
| 503 | `no_capacity`, `unavailable`; both include `Retry-After`, 5 seconds unless a Boat refusal or the tenant's Boat start limit names a longer wait |

Clients treat `usage_conflict`, `job_settled` and `job_unknown` as final for that report, not
retryable transport failures; so are a usage report's `job_conflict` (the job was replaced away
from that host) and a replace's 409 refusals. `sandbox_lost` is final for that job: its Boat sandbox stopped or
failed under the lease, and ck-motor must start a new `job_id`. `no_host_fits` is final for that
lease body: retrying it never succeeds while the registry is unchanged, so the client routes the
job elsewhere or asks for at most `max_vcpus`. `account_closed` is final for that account: it was
closed (the customer left) and gets no new lease, ever; see [Account closure](#account-closure).
Host retries after a consumed counter must use a new counter.

## Client endpoints

After routing, client authentication precedes Content-Type, size, JSON parsing and all state
access. A missing or invalid token returns 401 `unauthorized` even with no Content-Type, an empty
body or an oversized body; no totals or rows change. Once authenticated, grammar or bound
failures, including a path `job_id`, are 400 `bad_request` without writes.

### POST /v1/leases

The exact body fields, in canonical order, are:

```text
{job_id, vcpus, platform, net, workspace_key_hash, repo_hash,
 placement_hint, avoid_host, timeout_s, requester_jwk, allow_spill}
```

`allow_spill` is a required boolean, described under "Per-lease spill policy" below. No other
members are accepted.

The canonical lease hash uses this exact member order:

```text
{ job_id, vcpus, platform, net, workspace_key_hash, repo_hash, placement_hint, avoid_host, timeout_s, allow_spill }
```

It omits `requester_jwk`. `allow_spill` goes immediately after `timeout_s` with its exact boolean
value (`true` and `false` hash differently).
`test-vectors/runners/no-spill-lease.json` records `canonical_lease_bytes`, the exact canonical
JSON string hashed, for clients to byte-compare. A re-request must repeat the same `allow_spill`
value or receive 409 `job_conflict` wherever a differing lease hash conflicts.

`vcpus` is 1–64 and `timeout_s` 10–21600 (6 hours). A Boat sandbox takes only a lease whose
`timeout_s` is at most `BOAT_MAX_TIMEOUT_S` (7200 unless the operator lowers it); a longer lease
runs only on a registered host (see [Boat placements](#boat-placements)). `net` is boolean. There is no client `mem_mib` field:
the broker derives memory as `max(16384, 3584 × vcpus)` MiB for placement and the token.
`placement_hint` is JSON `null` or exactly `{host, vm}`, both strings following the common name
grammar, except that `host` may also carry the `boat:` prefix of a Boat placement, so a client can
send back the placement it was last given. A matching selectable warm placement is preferred; an
unusable hint, including any `boat:` hint, falls back to normal placement, except for the binding
explicit-only rule below. Other strings and the requester key follow the grammar table under "Common wire rules" above.

The operator-set D1 `hosts.explicit_only` flag defaults to false. Ordinary placement never uses
an explicit-only host, including warm/affinity matching and fallback, and excludes its ceilings
from `no_host_fits`. The only opt-in is a `placement_hint.host` equal to that host's registered
id. That hint is **binding**, even if its VM is missing or cold: only the named host may be used,
with the usual freshness, health, current-generation, free-capacity and `avoid_host` checks.
Warm selection stays within that host; without a warm match the reply has `vm: null`.
No other host or Boat is allowed, even with `allow_spill: true`.

Binding refusals apply in this order:

1. A binding host on the wrong platform is **422** `{"error":"no_host_fits","max_vcpus":0}`.
2. A retired or avoided binding host gives the existing retryable **503**
   `{"error":"no_capacity"}` with `Retry-After: 5`.
3. For any other active binding host whose recorded ceilings cannot ever fit the lease, the reply
   is **422** `no_host_fits`, with `max_vcpus` computed from that host alone and no `Retry-After`,
   even if its snapshot is full or stale. Unknown ceilings cannot establish a permanent refusal.
4. Otherwise, a full, stale, unhealthy, wrong-generation or missing snapshot gives **503**
   `no_capacity` with `Retry-After: 5`.

Binding policy is resolved even for retired or wrong-platform registry rows. Ordinary-host and
unknown-host hints retain their non-binding meaning. Existing jobs and Boat provisioning records
keep their replay semantics after a flag change; the flag governs fresh placement, not revocation.
The real-route vector
`test-vectors/runners/explicit-only-leases.json` pins successful opt-in and full-host refusal bytes.

`avoid_host` is required: JSON `null`, or a host id string matching
`^[A-Za-z0-9._-]{1,64}$` (no `boat:` prefix; a Boat sandbox is not a host). A body without the
key, or with any other value, is 400 `bad_request`. The named host is not selectable for this
lease, even when `placement_hint` names it; unless the hint is binding, placement proceeds through
the other hosts and then Boat spill exactly as if that host had failed selection. ck-motor sends the host it is spilling
away from, so a job leaving a busy machine is never placed back on it. Because the host stays
registered, a platform served only by the avoided host answers `no_capacity` (or spills), not
`platform_unsupported`. A value naming no registered host, and `null`, change nothing. The lease
hash covers `avoid_host`, so a re-request must repeat it exactly; a different value is 409
`job_conflict`.

After authentication and schema validation, checks proceed in this order:

1. Read the account's allowance, then claim `job_id` in D1. A claim belonging to another tenant is
   404 `job_unknown` and reveals none of that job's data. A closed account writes no new claim
   (see [Account closure](#account-closure)).
2. Check for an existing `TenantDO` row. A release marker takes precedence and gives 409
   `job_released`, even after settlement. Otherwise a settled job is 409 `job_finished`; a reserved
   job ever reported `admitted` or `started` is 409 `job_admitted`. Otherwise an identical lease hash
   returns 201 on the same reservation, with a fresh `run` token for this requester's key. A
   different hash is 409 `job_conflict`. These replies do not reserve again. With no row and no
   Boat provisioning record for the job, a closed account's lease is refused 403
   `account_closed`, with no placement, Boat call, provisioning record or reservation. Otherwise,
   after the broker deletes job records under the
   [retention rule](runners.md#retention), a lease whose `job_id` (UUIDv7) time falls at or before
   that tenant's deleted period is refused 404 `job_unknown`, so a deleted job id is never accepted
   again. The D1 claim from step 1 remains, as for any refusal.
3. Ask for placement. Binding explicit-only hints follow the rule above, without fallback.
   Otherwise, if no registered host, in any status, has the requested platform, return 400
   `platform_unsupported`. If hosts are registered for it but none is active and selectable
   (including when every one is retired), return 503 `no_capacity`, unless no candidate host could ever fit the lease (below), which is 422
   `no_host_fits`. Before returning a refusal, repeat the existing-job check so a concurrent
   successful lease wins over that refusal. When `allow_spill` is `true`, Boat spill is
   enabled, the platform equals `BOAT_PLATFORM` and `timeout_s` is at most `BOAT_MAX_TIMEOUT_S`,
   either 400 or 503 refusal is replaced by the
   spill path in [Boat placements](#boat-placements), and so is `no_host_fits` when a Boat size
   fits the lease.

   **Never fits (`no_host_fits`).** The candidates are every `status = 'active'` host of the
   lease's platform except explicit-only hosts and the one `avoid_host` names. Health, heartbeat
   age, generation and free
   capacity are ignored: they are temporary, so a host that could fit when idle and healthy
   keeps the answer at `no_capacity`. A candidate can fit when its registry per-job ceilings are
   both recorded and `max_job_vcpus ≥ vcpus` and `max_job_mem_mib ≥ max(16384, 3584 × vcpus)`.
   If any candidate's ceiling is unrecorded (NULL), the broker never concludes "never fits" and
   answers exactly as without this rule. With at least one candidate, all ceilings recorded and
   none able to fit, the reply is **422** `{"error":"no_host_fits","max_vcpus":N}` with no
   `Retry-After`. Without any candidate (no active host has the platform, or only the avoided host
   does, or all are explicit-only without a binding hint) the codes above apply unchanged. When
   the lease allows spill, Boat is available for its
   platform (enabled, configured, with a current image, and `timeout_s` at most
   `BOAT_MAX_TIMEOUT_S`) and a Boat size fits, the lease spills
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
19999 refuses. Unlimited, 64 vCPUs and 7200 seconds reserves 460800000; unlimited, 64 vCPUs and
21600 seconds reserves 1382400000. `test-vectors/runners/long-lease.json` pins a 21600-second
lease granted in full and the 400 `bad_request` (`field: "timeout_s"`) for 21601.

Placement and budget refusals leave no `TenantDO` job row and unchanged totals, but retain the D1
claim. The same tenant can retry after capacity returns or allowance increases; another tenant
still gets 404. Existing-job refusals never mutate its row or totals. Claims, job rows and
settlement records are deleted 13 months after they were written or settled
([retention](runners.md#retention)); a deleted job is unknown from then on.

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
The broker remembers the requester key of the job's latest run token; a
[replace](#post-v1leasesjob_idreplace) signs for that key. If a token is expired before
connecting, re-request before admission or use reattach; never
connect with a token whose `token_expires_at_ms` is not after the client's current time. Neither
refresh path, nor a replace, extends the original host-lost deadline or starts another execution.

Placement is not a capacity lock. On a full host, retrying the same lease keeps the same
placement; a job no host has admitted yet can be moved to another registered host with
[replace](#post-v1leasesjob_idreplace). The host releases the unused first reservation with a
zero-duration usage report, or it remains until the host-lost alarm settles proved elapsed time.

Gate priority is not a lease member: it travels only in the Run frame between ck-motor and the
tenant's runner, so the broker never sees it and placement ignores it.

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
`job_unknown`, and so does a job whose records were deleted under the retention rule. For a known
job the broker then decides, in this order:

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
settled job with 409 `job_finished` (or `job_released`) until its record is deleted 13 months
after settlement; from then on they, like reattach, answer 404 `job_unknown`.
`test-vectors/runners/settled-reattach.json`
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

### POST /v1/leases/{job_id}/replace

Moves a registered-host job that no host has admitted yet to another registered host, keeping the
same job, lease id, run limit and reservation. ck-motor uses it, for example, when the placed host can't start the job soon.

Authentication, path validation and the D1 claim check are exactly release's, and the body is
exactly `{}` with JSON Content-Type. Unknown, claim-only or another tenant's jobs return 404
`job_unknown`. For a known job, in this order:

1. A release marker: 409 `job_released`.
2. A settled job: 409 `job_finished`.
3. A job any heartbeat from its current host has reported `admitted` or `started`: 409
   `job_admitted`. A Boat job is admitted at booking, so it is never replaced.
4. Otherwise the job is placed again, under the rules of
   [Placement, deadlines and metering](#placement-deadlines-and-metering), with the original
   lease's platform, vCPUs, memory, workspace, `placement_hint` and `avoid_host`, and with every
   host the job has already been placed on excluded as `avoid_host` is. Only registered hosts are
   considered: replace never spills to Boat, whatever `allow_spill` said. A binding explicit-only
   hint stays binding, so such a job has no other host to move to.
5. If no host is selectable, the answer is **503** `{"error":"no_capacity"}` with
   `Retry-After: 5`, whatever placement's refusal was, and the job stays on its current placement
   unchanged. So is a job recorded before the broker stored what a replace needs (its requester
   key and lease placement inputs).

Success is **201** with exactly the lease reply fields, for the new host:

```text
{lease_id, job_id, placement:{host,vm}, endpoint, endpoint_spki_sha256,
 job_token, token_expires_at_ms, max_s, reserved_vcpu_ms}
```

`lease_id`, `max_s` and `reserved_vcpu_ms` are unchanged. `placement`, `endpoint` and
`endpoint_spki_sha256` are the new host's. `job_token` is a fresh `run` token for the new host,
bound to the requester key of the job's latest run token (the original lease's key, or the key of
the latest re-request). The reservation and totals don't change, and the host-lost deadline stays
the original lease's. Release afterwards still waits for the latest run token the broker issued,
for either host, to expire.

From then on the job belongs to the new host only, and the host it left must not run it:

- The old host's heartbeats that name the job list it in `ignored` and change nothing, whatever
  state they report. A host must stop, or never admit, a job its heartbeat reply lists in
  `ignored` (see the heartbeat endpoint).
- The old host's usage report for the job is refused 409 `job_conflict` and settles nothing.
- Only the new host's heartbeats advance `last_elapsed_ms` or mark the job admitted, so a
  host-lost settlement uses only the new host's proved elapsed time.

**If the old host admitted the job anyway**, typically because ck-motor connected with a run
token issued before the replace and the host admitted it before its next heartbeat, that run is
not billed: its usage report is refused, and the tenant pays only what the new host reports, or
what the host-lost alarm settles from the new host's elapsed time. The broker detects it when the
old host's heartbeat reports the job `admitted` or `started`: the job is listed in `ignored`, and
the broker logs `replaced_job_admitted_by_old_host` with the job id, that host and the current
host. The old host learns from `ignored` that it must stop the run. ck-motor calls replace only
after giving up on starting the job on the old host, so this needs a race with an in-flight
connection. `test-vectors/runners/replace-lease.json` pins a replace, the old host's heartbeat
reply listing the job in `ignored` and the refusal of its usage report.

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

The registry's `role` is `placement` by default or `monitor`. A monitor has its own signing key
and may call only the metrics route. Heartbeat and usage return 401 `host_auth_failed` before
consuming a counter. Placement excludes monitors even if an older heartbeat remains stored.
An explicit-only hint naming a monitor refuses with 503 `no_capacity` rather than falling back.
A role is fixed for a host ID, including after retirement and re-registration. The CLI refuses
a role change before writing; use a new host ID instead. The lease path does not re-read the
role when it signs a replay or reattach token for a job already on that host.

Register monitors with `node workers/runners/scripts/operators.mjs register-host <host_id>
<public_key_hex> <platform> --role monitor`, without endpoint or SPKI arguments. Monitors open
no listener and are never dialled; `hosts.endpoint` and `hosts.endpoint_spki_sha256` are both
NULL for monitors. Placement registration defaults to `--role placement` and keeps the five
positional arguments `<host_id> <public_key_hex> <endpoint> <endpoint_spki_sha256> <platform>`;
both endpoint columns must be non-null. A table CHECK enforces those role-specific shapes.
Both forms print SQL by default and require `--execute` to write.

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
   400 `bad_request`, with no `HostDO` storage or counter write. For metrics only, an oversized
   body returns 413 with an empty body instead.
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

### POST /v1/hosts/{host_id}/metrics

Hosts, including monitor-only machines, push every 60 seconds using the same signature and
per-host counter as heartbeat and usage. The limit is 16 KiB of raw UTF-8 body bytes. Success is
204 with no body. Malformed JSON or schema returns 400 `bad_request` with `field`; authentication
failures are 401 `host_auth_failed`. Size is checked before authentication, schema after it.
The route never updates heartbeat, placement capacity, availability or admission state. Missing
metrics never prevents a placement host from receiving jobs.

The strict body has exactly these members (nested objects are strict too):

```json
{
  "at_ms": 1800000000000,
  "cpu": { "threads": 32, "busy_pct": 41, "load1": 12.4 },
  "mem": { "total_mib": 128000, "avail_mib": 22000 },
  "disks": [{ "mount": "/", "total_gib": 900, "free_gib": 310 }],
  "tenants": [{ "vm": "vm-1", "state": "ready", "vcpus": 30, "mem_mib": 106496,
    "slots_total": 90, "slots_used": 41, "jobs_running": 12, "recovery": null, "detail": null }],
  "runners": [{ "name": "direct", "state": "ready", "slots_total": 16, "slots_used": 3,
    "queued": 0, "oldest_wait_s": 0, "detail": null }]
}
```

- `at_ms` is within 300000 ms of broker receive time, inclusive in both directions.
- `cpu.busy_pct` is an integer 0..100; `cpu.load1` is a non-negative finite number with at most two
  decimal places, or null when the platform has no load average (for example, Windows). The
  `load1` key is always required; Linux and macOS send a number. Decimal notation is allowed
  here, unlike other routes. All other numeric fields are
  non-negative safe integers. Exponent notation is not accepted.
- Each array has at most eight entries and may be empty. `mount` is 1..128 UTF-8 bytes with no
  Unicode control characters. `vm` matches `^[A-Za-z0-9._-]{1,64}$`.
- Tenant `state` is `ready`, `starting`, `recovering` or `stopped`. `recovery` must be null except
  for `recovering`, when it is `{ "step": string (1..64 characters), "since_ms": int, "attempts": int }`.
- Runner `name` matches `^[a-z0-9-]{1,32}$`; its `state` is `ready`, `draining`, `stopped` or `error`.
- Every `detail` is null or 1..200 UTF-8 bytes without Unicode control characters. It contains
  only host-generated errors or step names, never job or guest output. Do not include account,
  workspace, job or path data anywhere in metrics (the disk mount identifies a monitored filesystem).

HostDO holds the latest canonical body with broker `received_at_ms`. D1 `host_metrics_5m` uses
`floor(received_at_ms / 300000) * 300000` buckets and is pruned after 14 days by the bounded Cron
retention pass. It increments `samples`, retains maximum busy percentage and load, last busy
percentage and total memory, minimum available memory and disk free space, last total tenant
slots, and maximum summed tenant slots used and jobs running. Disk free space is the minimum
across the body's disks, or NULL with no disks; a bucket retains the minimum non-null sample.
Load retains the maximum non-null sample, or NULL when every sample in the bucket has null load.
`last_body` is the last accepted body serialized in the schema's key order without whitespace.
States and per-disk details are available in that body, not separate rollup columns.

`test-vectors/runners/host-metrics.json` pins accepted signed pushes with numeric and null load, the 413 boundary, and
bad-enum, overlong-detail and stale-clock refusals, produced by the real signer and parser.

### POST /v1/hosts/{host_id}/heartbeat

The exact body is:

```text
{free_vcpus, free_mem_mib, queued, warm:[{tenant, workspace_key_hash, vm}],
 running:[{job_id, state, elapsed_ms}], healthy, draining?}
```

Integers are non-negative safe integers. `healthy` is boolean. `warm` and `running` each have at
most 64 entries. `state` is exactly `reserved`, `admitted` or `started`. The heartbeat replaces
the host capacity snapshot, timestamped at broker receive time. `queued` is stored but does not
affect placement.

The optional final member `draining` is boolean; absent and `false` mean the same. Every other
unknown body member is still refused. Only the latest accepted snapshot stored by PlacementDO
controls draining: a delayed older heartbeat cannot set or clear it. The next accepted heartbeat
that omits `draining` or sends `false` clears the drain.

While that snapshot has `draining: true`, fleet placement treats the host as absent, including
warm affinity, and ordinary Boat spill proceeds normally. A lease whose `placement_hint.host`
names the draining host is a canary: the hint is binding, exactly as for `explicit_only`. It can
use that host, including a cold VM, but waits if the host is full or otherwise unavailable; it
never falls back to another host or Boat. Draining hosts still count for the registry's
`no_host_fits` / `max_vcpus` ceiling check because draining is temporary. A job only a draining
host can fit receives retryable `no_capacity` with `Retry-After`, not `no_host_fits` (and may
spill when allowed). The independent operator `explicit_only` override is unchanged.

`warm` lists the tenant workspaces the host holds **now**, after eviction, most-recent first. A
host holding more than 64 reports only the 64 most recent workspaces.

`free_vcpus` and `free_mem_mib` answer one question for placement: can this host take one more job
of a given size now? The broker places a lease on a host only if `free_vcpus ≥ vcpus` and
`free_mem_mib ≥ max(16384, 3584 × vcpus)`. It reads them for nothing else: billing, `no_host_fits`
and pricing use the registry ceilings and the usage reports instead. A host reports them for the
machine that runs its jobs:

- `free_vcpus` is the tenant's admission capacity in lease vcpus: its budget (`max_weight`, or
  `vcpu_ceiling` when that is unset) minus the requested `vcpus` of every lease the host counts. A
  lease counts while it is reserved, admitted or started and within its maximum duration. It is 0
  while the tenant's wake is pending or its start was refused, and 0 while the host refuses new
  work for lack of storage.
- `free_mem_mib` is the memory available to the tenant machine that runs jobs, not to the host:
  the tenant guest's available memory as its runner reports it, floored to MiB, minus the host's
  configured tenant memory reserve (rounded up to MiB), and 0 if that would be negative. A host
  without a valid capacity configuration reports 0. A stopped or suspended tenant reports its full
  RAM minus the reserve, so the lease that wakes it can be placed. A running tenant without a fresh
  sample (older than 30,000 ms, or a failed probe) reports 0.
- A host with several tenants reports, for each field independently, the maximum over its tenants.

The broker subtracts each placed job's vCPUs until the host's latest accepted snapshot lists it
in `running` in any state (`reserved`, `admitted` or `started`), and its memory until `admitted`
or `started`, from that host's reported free capacity before comparing (see the in-flight ledger under
[Placement, deadlines and metering](#placement-deadlines-and-metering)), so several leases placed
within one heartbeat interval don't all pass against the same values. The figures are still a
snapshot, not a lock: the host's own admission is the real limit, and a lease it can't take is
refused before Run with a fast `hosted_host_full`, which clients retry like `no_capacity`.

Running entries are processed in order. An unknown, claim-only or other-host job is listed in
`ignored` without failing siblings or writing `TenantDO` storage. That includes a job
[replaced](#post-v1leasesjob_idreplace) away from this host. A host must stop a job its reply
lists in `ignored`, and must never admit it. A settled job placed on this
host has no effect. A reserved job on this host updates
`last_elapsed_ms = max(last_elapsed_ms, min(elapsed_ms, max_s × 1000))`. States `admitted` and
`started` permanently mark it admitted; later `reserved` does not clear that flag. This prevents
lease re-request and replace, but not reattach.

Success is **200**. When the host's stored latest snapshot after this heartbeat is draining, the
reply is exactly `{"ignored":[<job_id>…],"closed_subjects":[<subject>…],"draining":true}`, in that
member order. Otherwise it is exactly `{"ignored":[<job_id>…],"closed_subjects":[<subject>…]}`,
unchanged for hosts that never send the field. The echo describes the stored snapshot even when
this heartbeat finished after a newer one. A host waits for the `draining: true` echo before
waiting for its existing leases to finish. `ignored` lists strings in entry order.

`closed_subjects` lists the closed accounts (see [Account closure](#account-closure)) among the
tenants this heartbeat's snapshot reports: each `warm` entry's `tenant`, and the owner of each
`running` job the broker holds a D1 claim for (listed or not in `ignored`). Each is the same
`acct:<account ULID>` string the broker uses for tenants in placement and `warm` entries, sorted,
without duplicates; a closed account the snapshot does not report is never listed, and the array
is empty when none is closed. For each listed subject the host deletes that tenant's data (its
VM disks, warm state and cached workspaces) once that tenant's last lease on the host has ended;
it keeps running the tenant's current jobs to their end. Closure is permanent, so a listed
subject never needs to be undone. A valid heartbeat from a retired host still updates elapsed
time and snapshots.

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
   consumed; the job's host and totals are unchanged. A job [replaced](#post-v1leasesjob_idreplace)
   away from the signing host is instead 409 `job_conflict`, with the same effect: that host
   must not have run it, and its report is never billed.
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

Selectable hosts have role `placement`, are active, platform-matching, healthy, no more than 60000 ms since heartbeat,
have a snapshot for their current registry generation, and have sufficient **effective** free
vCPUs and memory (below). Explicit-only hosts are excluded unless named by the binding hint
described under [POST /v1/leases](#post-v1leases); a binding hint restricts every selection step
to that one host. Selectable hosts are ordered by most effective free vCPUs, then most effective
free memory, then bytewise host id. Prefer the placement hint if it names a selectable warm entry
matching tenant and `workspace_key_hash`. Otherwise take the first host in that order holding a
matching warm entry, with its bytewise lowest matching VM id. Without a warm match, take the first
selectable host in that order with `vm: null`. `repo_hash`, `net` and `queued` do not affect
placement.

**In-flight ledger.** A heartbeat's free figures don't count a job placed on the host since that
sample. So that a burst of placements inside one heartbeat interval spreads over hosts instead of
filling one, the placement object keeps a ledger: when it answers a placement a lease will use
(a new lease, or a replace) with host H, it records `{job_id, host: H, vcpus, mem_mib}` in the
same storage transaction as the decision. A host's effective free vCPUs and memory are its last
accepted heartbeat's `free_vcpus` and `free_mem_mib` minus the resources its live entries still
hold. A job listed in that host's latest accepted `running` list in any state (`reserved`,
`admitted` or `started`) no longer holds vCPUs: the host already counts its reservation.
Memory stays held until `admitted` or `started`, because a queued job uses no guest memory yet.
A job not reported by that host still holds both resources. The fit check and ordering both use
these effective figures. An entry is removed on any of these events (the 1024-entry cap below can also evict one):

- on the first heartbeat from H whose `running` list shows the job `admitted` or `started`, the
  point from which the host's own figures count both resources. A heartbeat that shows the job
  `reserved`, or doesn't list it, keeps the entry for memory or both resources respectively;
- when the job settles (usage report or alarm), is released, or is replaced away from H, and
  when the lease that asked for the placement doesn't commit it (the job already existed, or the
  budget ran out while the token was signed);
- when the job's latest run token expires without such a heartbeat: a host refuses an expired
  token, so the job can no longer be admitted. When a re-signed run token's refresh reaches the
  placement object, it moves the entry's expiry to the new token's, and brings the entry back if
  it had expired, unless the host's latest heartbeat already shows the job admitted or started.

Consistency: placement decisions and ledger writes are serialized in the one placement object and
survive its eviction. Releases on settlement, release, replace and re-sign are sent by the job's
tenant object after its own state change commits; a failed message is logged and not retried. A
lost release leaves an entry until its stored expiry, so its host looks fuller than it is for up to
300 seconds. A lost refresh can leave an entry expiring before the newest token does, so that host
can look freer than it is. The ledger holds at most 1024 entries: before inserting into a full
ledger, the broker drops the existing entry with the earliest expiry. Effective figures are
estimates, not a lock: the host's own admission remains the real limit.

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
and cleaned up. Each tenant may have at most `BOAT_MAX_SANDBOXES` sandboxes whose machines may be running
(confirmed stopped sandboxes do not count), and **a sandbox serves at most one live lease**. Placement hints never select a sandbox.

**Cleanup and provider debugging.** Idle ready sandboxes with no claim or live lease are
**deleted directly** after `BOAT_IDLE_MS`, without stopping or taking a stop snapshot. The broker
never resumes, restarts or recovers a stopped sandbox; stopped entries left by an older broker
are deleted by cleanup or reconciliation. Delete intents are persisted in D1 before Boat DELETE.
A direct deletion of a running sandbox holds capacity until deletion completes.
A `lost` finish retires the sandbox: the client cannot prove its job stopped, so the sandbox is
deleted rather than booked again. Other finish outcomes retain ordinary ready reuse.

A start still unready after `START_TIMEOUT_MS` (5 minutes) gets the bounded timeout diagnostic,
then is stopped and marked `kept_for_provider` when fewer than two boxes are already kept across
all tenants. Otherwise it is stopped and deleted normally. D1 `provider_sandboxes.kept_at_ms`
persists the reservation. A kept sandbox never takes a job and frees running capacity only once
its stop is confirmed. The broker logs `boat_sandbox_kept` with `{sandbox_id, incarnation,
started_at_ms, timed_out_at_ms}` and no tenant or workspace. Cron deletes it 24 hours after it
was kept, or earlier after `node scripts/operators.mjs release-kept-sandbox <sandbox_id> --execute`
queues a deletion request. The public alert `boat_sandbox_kept` remains active until deletion
completes, so the operator can send its id to Boat before its logs are deleted.

**Stuck stops.** A delete-bound stop still unconfirmed after 15 minutes is checked against Boat
again. With neither a claim nor a live lease, any fresh state other than `running` or `starting`
allows direct deletion; running or starting resends the stop. A claim or lease appearing during
the read blocks deletion. The switch logs `boat_stop_stuck_deleted` alongside `boat_stop_stuck`.
A kept box never uses this deletion fallback. A stopping delete-bound sandbox only frees its cap
slot after a stuck report and a post-deadline read of `archiving` or an ended state. A kept box
requires stop confirmation. Ordinary confirmed-stopped deletions do not count toward the cap.

**Per-lease spill policy.** The required `allow_spill` member is strictly a JSON boolean; a body
without it, or with any other value, is 400 `bad_request` before any job claim or Boat call.
`false` requires a registered host: when placement refuses,
the broker returns that refusal unchanged (including its status, body and Retry-After), never
claims or creates a Boat sandbox, makes no Boat calls and leaves no spill record for that lease.
For example, a full registered fleet returns 503 `no_capacity` with Retry-After `5`, and a job
larger than every registered host's per-job ceiling returns 422 `no_host_fits` whose
`max_vcpus` counts registered hosts only.
`true` allows ordinary spill behavior. This is per lease, not per tenant, so
callers sharing a fleet account can choose independently.
It does not override the Boat enablement, platform, size or budget gates.

**Eligibility.** The lease's platform equals `BOAT_PLATFORM` (`linux-x64`), its `timeout_s` is at
most `BOAT_MAX_TIMEOUT_S` (default 7200; the deploy preflight requires 10 to 7200, because Boat's
own auto-stop is capped at three hours and must outlast the run and its deadline grace), and
`BOAT_SIZES`
(the usable `{type, vcpus, mem_mib}` measured inside each Boat size) has a size with
`vcpus ≥ job.vcpus`; the smallest such size is used. Memory does not decide eligibility. No
fitting size is 503 `no_capacity`, or 422 `no_host_fits` when no registered host could ever fit
the lease either (see step 3 of the lease order). A lease above `BOAT_MAX_TIMEOUT_S` never goes to
Boat: it gets the registered-host answer unchanged, 503 `no_capacity` when no host
has room now, or 422 `no_host_fits` with `max_vcpus` counting registered hosts only when no host's
ceiling could ever fit it.

**Order.** The budget candidate is computed first, exactly as for hosts; below 10 seconds the
lease is 402 `budget_exhausted` and no Boat call is made. While the shared Boat breaker is open
(after a Boat 429 or 503), the lease is 503 `no_capacity` at once with that wait as Retry-After.
Then, in one transaction that computes the budget candidate again (402 `budget_exhausted` below
10 seconds), a provisioning record keyed by `job_id` claims a pool slot. A sandbox records the
`workspace_key_hash` of the lease last booked on it; an entry with no recorded workspace matches
nothing, and the hash is compared for equality only. Placement takes (a) a running, ready,
free sandbox that last served the same workspace, oldest first; (b) any running, ready, free
sandbox of the right size, oldest first; or (c) a fresh sandbox under the cap. A lease never waits
on a resume. A ready sandbox whose remaining Boat TTL is insufficient is retired and the lease
is placed afresh; it is never stopped and restarted. Placement hints never select a sandbox.

Creates count against the tenant's hourly start limit: its operator-set account limit (1 to 1000),
otherwise `BOAT_MAX_STARTS_PER_HOUR` (6). A fresh claim holds a persisted start reservation;
admission includes reservations plus sent starts in the last hour. The send fence counts the
start and releases its reservation. Unsent claims ending release their reservation. A sent create
is never refunded on failure or a lost reply, but exact-key replays count only once. Daily D1
start reporting counts sent creates as well. A ready sandbox may be reused at either limit.
Past the hourly limit, with no ready sandbox, the lease gets 503 `no_capacity` with Retry-After
until a start leaves the hour; lack of a cap slot is also 503 `no_capacity`.

**Provisioning hold.** The claiming transaction reserves a provisioning hold of
`min(vcpus × 60000, vcpus × max_s × 1000)` vCPU-ms, the minimum a booked Boat job is charged; it
appears in the tenant's `reserved_vcpu_ms`. Booking turns the hold into the job's reservation. A
record that ends without a booking (it expired, Boat refused the start, or the budget was gone)
**charges the hold** if a Boat create was sent for it: it moves to `settled_vcpu_ms`,
reason `lost`, on the broker clock, and like every Boat settlement is never counted in the usage
aggregate. This includes a create whose reply was lost: Boat may have started the machine. A record that started nothing releases its hold.

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
fetched once per created sandbox incarnation, and every re-request and
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

**Image obligations** (the tenant sandbox image):

- Tenant sandboxes are created confined (`noEnv`), so the snapshot must not depend on any account
  secret, and is built from a `noEnv` sandbox through the clean image pipeline.
- The broker writes `/etc/motor/tenant/id` after every create, once Boat reports the
  sandbox hydrated, root-owned `0444`, holding the sandbox id as ASCII with no newline: it stages
  the id with Boat's file API at `/tmp/ck-motor-tenant-id`, then runs `sudo install -D -m 0444 -o
  root -g root /tmp/ck-motor-tenant-id /etc/motor/tenant/id && rm -f /tmp/ck-motor-tenant-id &&
  cat /etc/motor/tenant/id` and requires the printed id back. The runner reads it, refuses all
  connections while it is missing, and checks the token's `aud` equals `boat:<id>`.
- **Compile-cache provisioning.** The broker installs the sandbox's compile-cache provisioning
  after every create, once Boat reports the sandbox hydrated. Running ready sandboxes,
  including those serving a job, are brought up to the tenant's current key version without a
  restart: the alarm schedules a best-effort check every 60 seconds, and convergence may take
  longer, subject to the attempts below. For already-running sandboxes this refresh does not hold
  up booking, reserve a start, or count against an hourly or daily start limit. The runner re-checks
  the files on every admission. A stop, retirement or deletion supersedes an in-flight install;
  its completion cannot revive the sandbox.
  A guest reboot the broker does not see clears `/run`; an unchanged recorded version cannot
  detect that loss, so the cache is then unavailable until the next create.
  - **Paths and modes.** `/run/motor/cache/` is a root-owned (`root:root`) `0700` directory on
    tmpfs. It holds two root-owned files: `/run/motor/cache/read-key`, mode `0400`, and
    `/run/motor/cache/config.json`, mode `0444` (plus, only during a write, its temporary file).
    Neither file, nor any staging copy of one, is ever placed in `/etc` or anywhere Boat saves in
    a snapshot: Boat archives `/etc` in snapshots and swaps it late after a restore. The broker
    stages content through Boat's files API in a private `0700` directory with a random name
    under `/tmp`, which Boat never saves in snapshots, and removes the staging copy right after
    the install.
  - **`read-key`** is one UTF-8 JSON object with exactly the members `generation` (string),
    `access_key_id` (string) and `secret_access_key` (string): the read-only OVH credential of the
    sandbox tenant's cache tenant. The bucket policy confines that credential to exactly the
    tenant's store prefix and grants it no write. A read-write key is never written to a Boat
    sandbox. A raw key, or any other shape, is invalid and leaves the cache unavailable.
  - **`config.json`** is one UTF-8 JSON object with exactly the members `generation` (string),
    `tenant` (string, the cache tenant's subject, e.g. `acct:<ULID>`), `endpoint` (string),
    `region` (string), `bucket` (string), `prefix` (string), `storage_class` (string) and
    `enabled` (boolean). `prefix` equals `"tenant/" + tenant.replace(":", "-") + "/"`, e.g.
    `tenant/acct-01KXJJHK4V6DB42W1YAN2XD9QN/`. The broker installs a cache tenant's files only
    into that tenant's own sandboxes.
  - **Generations.** `read-key` and `config.json` carry the same `generation` value, an opaque
    string the runner only compares for equality and never orders. A generation is immutable
    except for `enabled`: a new key or any other changed config member is installed under a new
    `generation`, and `enabled` alone may be flipped in place. The runner activates a set only
    when both files parse and share one `generation`. A mixed set leaves the previous complete
    generation active, and a reused `generation` with changed contents makes the cache
    unavailable.
    The broker records the attempted and verified installed version per sandbox incarnation,
    comparing the pair (`generation`, `enabled`). The same version is not rewritten on later
    passes, including after its install attempts fail; a changed version gets its own attempts.
    A failed read of the tenant's desired cache state logs `boat_cache_key_install_failed` but
    changes neither installed files nor the recorded version. The broker claims no version for
    a failed read, so the next cleanup pass retries the read.
  - **Disable and removal.** A disabled tenant key is delivered with `enabled: false` under the
    same generation, disabling cache use without removing the credential. A deleted or missing key,
    or a closed account, removes both files and their temporary files, including on a running
    sandbox; the broker records the no-key state so later passes do not repeat the removal.
    `CACHE_KEYS_ENABLED` other than `"true"` does neither installation nor removal and preserves
    existing files. Turning delivery back on compares the current tenant state again.
  - **Atomic writes, key first.** Each file is written to a temporary file in
    `/run/motor/cache/` with its final owner and mode, then renamed over its final name. The
    broker renames `read-key` before `config.json`.
  - **Read-back.** After both renames, the broker runs
    `sudo sha256sum /run/motor/cache/read-key /run/motor/cache/config.json` through Boat's exec
    API and requires each printed digest to equal the SHA-256 of the bytes it wrote. For a
    retryable failure it repeats the whole write, at most 3 attempts per version mismatch per
    incarnation; an unanswered Boat call or a 404 ends retries early. After that it logs `boat_cache_key_install_failed` and attempts to remove the files. Staging directories whose
    cleanup failed are retained for removal in a later pool step of the same incarnation. Neither
    installation nor cleanup failure blocks jobs.
  - **Cache tenant versus sandbox.** The runner takes the cache-tenant id only from `tenant` in
    `/run/motor/cache/config.json`, never from `/etc/motor/tenant/id`, which keeps holding the
    sandbox id for the admission audience `boat:<id>`. Two sandboxes with different ids can share
    one cache tenant and read the same prefix.
  - **Never gates readiness.** Missing, invalid or unverified cache files make the runner report
    `cache_unavailable` and build uncached. They don't delay `/run/motor/endpoint-ready`, don't
    hold back booking, and aren't a `sandbox_lost`.
- The runner listens only on `127.0.0.1:9443`.
- Once the runner listens with its signing keys loaded, the image writes `/run/motor/endpoint-ready`
  holding the sandbox id (the content of `/etc/motor/tenant/id`, a trailing newline allowed). The
  broker reads it with the fixed command `cat /run/motor/endpoint-ready` through Boat's exec API
  after every create, and books no lease onto the sandbox until it names this
  sandbox. `/run` is cleared at boot, so the marker always belongs to the current start. The
  endpoint removes the marker whenever it cannot admit jobs (its runner disk full, say) and writes
  it again once it can: before booking a sandbox that is already running from an earlier lease,
  the broker reads it once more and books only if it still names the sandbox; otherwise the
  sandbox is stopped and deleted.
- sshd sets `AllowTcpForwarding local`, `PermitTTY no`, `AllowAgentForwarding no`,
  `X11Forwarding no` and `PermitTunnel no`, and the boot unit empties `authorized_keys` on every
  boot.
- Nothing in the sandbox signs to the broker: no report key, heartbeat or usage report.

## Account closure

When a customer leaves, an operator closes the account (`close-account`, see
[runners.md](runners.md#close-an-account)); the broker records `closed_at_ms` on the account's D1
`allowances` row. Closure is permanent: there is no reopen.

- **New leases are refused.** A `POST /v1/leases` for a `job_id` the account's `TenantDO` holds
  neither a job nor a Boat provisioning record for is **403** `{"error":"account_closed"}`, with no
  `Retry-After`. It is final: the client must not retry it here. No placement, Boat call,
  provisioning record, reservation or D1 claim results.
- **Existing jobs run to their end.** A re-request of a job already placed or provisioning keeps
  its normal answer (201 with a fresh `run` token, or 202 while its sandbox starts, then 201), and
  reattach, release, finish, host heartbeats and host usage reports for existing jobs work as for
  an open account, so every job settles normally. `GET /v1/tenant` keeps answering.
- **Boat sandboxes are retired.** The broker's Cron tells the account's `TenantDO` it is closed.
  From then on the tenant never claims or creates a sandbox again (even for a lease read as open
  just before the closure, which is refused 403 `account_closed`). Every sandbox no lease or
  provisioning record holds is stopped and then deleted at once; one under a live lease, or
  claimed by a provisioning record, is stopped and deleted as soon as that lease or record ends.
- **Hosts delete the tenant's data.** Heartbeat replies list the closed tenants a host reports in
  `closed_subjects` (see the heartbeat endpoint above). The host deletes each listed tenant's
  disks after that tenant's last lease on the host ends.

## Pinned conformance evidence

Registered-host placements also increment the per-host D1 counter `host_grants_daily`
(`day` as UTC `YYYY-MM-DD`, `host_id`, `grants`) when TenantDO confirms the winning assignment,
at the same point as `host_placement_daily`. Selection alone, refused placement, dropped losers,
keep retries, lease replay and token re-signing do not increment it. Boat is not a registered host
and does not contribute. The counter contains no tenant, workspace or job data.

`test-vectors/runners/job-tokens.json` pins both token purposes and their header/claim bytes,
the fixed signing seed and its derived public key. `test-vectors/runners/job-tokens-workspace.json`
pins both purposes with `JOB_TOKEN_WORKSPACE_HASH="true"`, using the same deterministic signer
inputs and appending the lease's workspace hash after `purpose`. The existing vectors keep the
switch off. `test-vectors/runners/lease.json` pins the lease request and exact reply, including
the lease id and SPKI hash.
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

`test-vectors/runners/account-closure.json` pins a closed account: its lease refused with the
final 403 `account_closed` (no `Retry-After`), and a signed heartbeat whose snapshot reports warm
entries for that closed account and for an open one, with the exact reply bytes
`{"ignored":[],"closed_subjects":["acct:<closed account>"]}`. A third, closed account the host
does not report stays out of the reply.

`test-vectors/runners/replace-lease.json` pins a replace of the lease vector's job onto a second
host (the 201 reply and its `run` token for that host), the old host's signed heartbeat that still
reports the job `started` with the reply `{"ignored":["<job_id>"],"closed_subjects":[]}`, the old
host's signed usage report refused with 409 `job_conflict`, and the tenant read showing the job on
the new host. `test-vectors/runners/long-lease.json` pins a 21600-second lease granted in full on
an unlimited allowance and the same body at 21601 refused with 400 `bad_request`, `field:
"timeout_s"`.

`workers/runners/test/vectors.test.ts` and `workers/runners/test/boat-vectors.test.ts` compare real
implementation output to those committed bytes. `workers/runners/test/job-token.test.ts`, `workers/runners/test/jwks.test.ts`,
`workers/runners/test/host-auth.test.ts`, `workers/runners/test/usage.test.ts`,
`workers/runners/test/rerequest.test.ts`, `workers/runners/test/replace.test.ts`,
`workers/runners/test/placement.test.ts` and `workers/runners/test/lease-concurrency.test.ts`
exercise the corresponding credentials, state transitions and accounting invariants. See
[runners.md](runners.md#tests-and-wire-vectors) for the full explicit test file list.
