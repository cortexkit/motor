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
`Nonces::verify` prunes timestamps older than twice the clock-skew window; an idle
host can also call `Nonces::prune` explicitly.

## Contract fixtures

The broker repository, `cortexkit-cloud`, owns `docs/runners-contract.md` and
`test-vectors/runners/`. Its pinned revision has not yet been supplied. For now,
`contracts/interim-pins.txt` is a verbatim excerpt of the reviewed interim Pins,
**not a new canonical contract**. `contracts/vectors/interim.json` is generated
from those formats with public, deterministic test seeds. The generator spells
out signing inputs independently of the library authentication functions.
`contracts/digest-manifest.json` records provenance and hashes. The digest test
also checks inventory completeness, so an omitted mirror file is an error.
`contracts/lease-details.txt` mirrors the subsequently supplied design excerpt and
confirmed contract clarifications. It supersedes the original excerpt's unkeyed
hashes and no-release wording. Those updates are confirmed contract rules, not
interim behavior; the canonical repository revision is still pending.
Replace the interim material with CLOUD's pinned contract/vectors before hosted
client integration merges; do not treat these fixtures as CLOUD approval.

Regenerate `contracts/vectors/interim.json` and the manifest hashes with:

```sh
cargo run -p motor-lease --example generate-interim-vectors -- --write
```

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

The config JSON contains `worker_bearer`, `subject` (`acct:<ULID>` or `org:<ULID>`),
`placement` (`host`, `vm`), `endpoint`, `endpoint_spki_sha256`, `kid`, a base64url
32-byte broker-only `signing_seed`, optional `announced_keys` (public JWKS keys),
and `registered_hosts` (`host`, `public_key` as a public requester-shaped JWK).
Never give the broker seed to `motor-host`, the job-executing host agent: possession
would let it mint its own leases. The endpoint is that separately run host's TLS
endpoint; this loopback control's HTTP listener does not terminate TLS.
The first stdout line gives its bound address; credentials are never printed.

`--refuse CODE` refuses leases with HTTP 403 before creating a reservation.
`--max-s SECONDS` simulates a budget-shortened runtime (including below 10 s).
The reservation remains `vcpus * max(requested timeout_s, 10) * 1000`, the formula
in `contracts/interim-pins.txt`, rather than shrinking with the runtime cap.
Model tests can set both knobs directly without a server or clock delays.

Workspace and repository hashes are opaque caller-supplied values. The broker
checks exactly 43 base64url characters and compares equality; it cannot check the
HMAC without the caller's private per-machine salt. No path-hashing helper is
provided here. ck-motor owns HMAC-SHA256 derivation using the `ck-ws-v1\n` and
`ck-repo-v1\n` prefixes described in `contracts/lease-details.txt`.

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
job id. Repeat release returns the original 202 reply unchanged, including after
release settlement. Other settled jobs get 409 `job_finished` on release, and
reattach after any settlement gets 409 `job_finished`. The in-memory model's
`FakeBroker::settle_due` and the executable's timer process release and lost-host
deadlines.

Run `cargo test -p motor-lease` for the digest, generated vector, authentication,
broker state-machine and real-loopback route tests.
