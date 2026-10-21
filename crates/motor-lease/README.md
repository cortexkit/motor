# Hosted leases

`motor-lease` is an I/O-free library: it verifies compact job JWSs with a supplied
JWKS cache, binds requester keys, verifies one-use challenges and signs/verifies
host HTTP messages. Clocks are explicit. The caller owns tenant binding, durable
nonce/counter state, JWKS fetching, admission and all transport. Persist a counter
before signing and serialize host requests; retry bodies unchanged with a fresh
signature. `KeySet::refresh` filters unsupported, malformed and conflicting keys,
retaining usable Ed25519 keys; an error or empty usable result keeps the last good
set. This lets future key types coexist without blocking Ed25519 rotation.
`verify_token` requires the expected purpose and refuses lifetimes over 300 s.
`KeySet::should_refetch` records fetch attempts and requests a refresh every ten
minutes or for an unknown signing key at most once a minute. Call it before each
fetch attempt, then pass the fetched result to `refresh`.

`Nonces::verify` prunes timestamps older than twice the clock-skew window; an idle
host can also call `Nonces::prune` explicitly.

## Contract fixtures

The broker repository, `cortexkit-cloud`, owns `docs/runners-contract.md` and
`test-vectors/runners/`. `contracts/cloud/` mirrors seven canonical files
verbatim: the contract and `no-spill-lease.json` at commit `95fd2d1`,
`lease.json` and `boat-lease.json` at `3d70bcf`, and the other vectors at
`00e688e`. `contracts/digest-manifest.json` records source paths,
revision and SHA-256 values. The digest test checks every file and inventory
completeness. The conformance tests compare tokens, signing bytes, request bodies
and settlement replies directly to these external vectors; they never regenerate
or rewrite the canonical bytes.

`no-spill-lease.json` pins the ordinary lease-hash projection, excluding
`requester_jwk` and including an explicitly false `allow_spill` after `timeout_s`.
It also records the real broker's `503 no_capacity` and `Retry-After` when only a
full registered host is selectable, even with Boat enabled. Existing vectors
retain their bytes and digests; absence of `allow_spill` is distinct from either
boolean in a lease hash, so re-requests preserve both presence and value.

## Fake broker test executable

`motor-fake-broker` is a loopback HTTP test server, not a production broker. It is
refused for the `main` instance (also enforced by the pure `FakeBroker` model).
It serves the lease, reattach, release, heartbeat, usage and public JWKS routes. Reservations
and counters are in memory; restart is a test reset. It settles expired unreported
jobs as `host_lost`. No job bytes are transported. Machine-worker authentication
is an exact configured test bearer mapped to a subject, not CK account acquisition.

```sh
cargo run -p motor-lease --bin motor-fake-broker -- \
  --runner-instance lease-test --config /path/to/test-config.json \
  --listen 127.0.0.1:0 --max-s 30
```

The config JSON contains `worker_bearer`, `subject` (`acct:<ULID>`),
`placement` (`host`, `vm`), `endpoint`, `endpoint_spki_sha256`, `kid`, a base64url
32-byte broker-only `signing_seed`, optional `announced_keys` (public JWKS keys),
and `registered_hosts` (`host`, `public_key` as a public requester-shaped JWK).
Never give the broker seed to `motor-host`, the job-executing host agent: possession
would let it mint its own leases. The endpoint is that separately run host's TLS
endpoint; this loopback control's HTTP listener does not terminate TLS.
The first stdout line gives its bound address; credentials are never printed.

`--refuse CODE` refuses leases with HTTP 403 before creating a reservation.
`--max-s SECONDS` simulates a budget-shortened runtime (including below 10 s).
The reservation is `vcpus * granted max_s * 1000`. Model tests can set a finite
allowance to exercise budget shortening and the below-10 refusal; `--max-s` can
also deliberately issue a below-10 cap as a hostile-client test control.

Workspace and repository hashes are opaque caller-supplied values. The broker
checks exactly 43 base64url characters and compares equality; it cannot check the
HMAC without the caller's private per-machine salt. No path-hashing helper is
provided here. ck-motor owns HMAC-SHA256 derivation using the `ck-ws-v1\n` and
`ck-repo-v1\n` prefixes described in `contracts/cloud/runners-contract.md`.

## Delayed release

`POST /v1/leases/{job_id}/release`, authenticated by the same tenant's machine
bearer, takes exactly `{}` and returns 202 `{job_id, release_at_ms}`. The deadline
is the latest **run** token's expiry plus 60 s; attach tokens do not extend it.
While pending, both reattach and lease re-request return 409 `job_released`, so a
new token cannot outlive the scheduled zero settlement. A signed heartbeat from
the job's registered placement host, naming that job as `admitted` or `started`,
cancels release permanently: the job has reached admission and can bill normally.
Even a later `reserved` report cannot undo that admission. The original
still-valid token can connect after release is requested.
Releasing an admitted job gets 409 `job_admitted`.

At the deadline, an unadmitted job settles at 0 with reason `released`. That job id
is permanently closed to lease requests (409 `job_released`); a new run needs a new
job id. Repeat release returns the original 202 reply only before settlement.
After release settlement, release returns `job_finished` and reattach still
returns `job_released`; that marker takes precedence over other settled status.
A host admitting a released job must report an admitted/started heartbeat before
`release_at_ms`, or the broker settles zero and rejects later usage as `job_settled`.
The in-memory model's `FakeBroker::settle_due` and executable timer process release
and lost-host deadlines.

Run `cargo test -p motor-lease` for the digest, canonical conformance, authentication,
broker state-machine and real-loopback route tests.
