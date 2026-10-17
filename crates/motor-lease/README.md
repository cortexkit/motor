# Hosted leases

`motor-lease` is an I/O-free library: it verifies compact job JWSs with a supplied
JWKS cache, binds requester keys, verifies one-use challenges and signs/verifies
host HTTP messages. Clocks are explicit. The caller owns tenant binding, durable
nonce/counter state, JWKS fetching, admission and all transport. Persist a counter
before signing and serialize host requests; retry bodies unchanged with a fresh
signature. `KeySet::refresh` retains the last good set on an error or empty set.

## Contract fixtures

The broker repository, `cortexkit-cloud`, owns `docs/runners-contract.md` and
`test-vectors/runners/`. Its pinned revision has not yet been supplied. For now,
`contracts/interim-pins.txt` is a verbatim excerpt of the reviewed interim Pins,
**not a new canonical contract**. `contracts/vectors/interim.json` is generated
from those formats with public, deterministic test seeds. The generator spells
out signing inputs independently of the library authentication functions.
`contracts/digest-manifest.json` records provenance and hashes. The digest test
also checks inventory completeness, so an omitted mirror file is an error.
Replace the interim material with CLOUD's pinned contract/vectors before hosted
client integration merges; do not treat these fixtures as CLOUD approval.

Regenerate `contracts/vectors/interim.json` and the manifest hashes with:

```sh
cargo run -p motor-lease --example generate-interim-vectors -- --write
```

## Fake broker test executable

`motor-fake-broker` is a loopback HTTP test server, not a production broker. It is
refused for the `main` instance (also enforced by the pure `FakeBroker` model).
It serves the lease, reattach, heartbeat, usage and public JWKS routes. Reservations
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

Run `cargo test -p motor-lease` for the digest, generated vector, authentication,
broker state-machine and real-loopback route tests.
