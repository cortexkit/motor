# Boat spill-provider environment probe

Operator-only, disposable measurement harness. Python 3.10+ and its standard
library are the only local requirements. Portable tests prove parsing and cleanup
plumbing, **not** the behavior of a Boat sandbox or motor-runner.

From the repository root, with a key reader that emits **only** the Boat API key:

```sh
set -o pipefail
<key reader> | python3 script/boat-probe/probe.py --key-stdin \
  --output .cortexkit/internal-docs/research/boat-live --max-minutes 30
```

Alternatively set `BOAT_API_KEY` yourself and omit `--key-stdin`. Never put the
key in argv, a committed file, a log or a shell with `set -x`. The harness removes
the environment variable after reading it, never sends it into the sandbox and
redacts it from API errors and saved output. The key needs sandbox create, read,
exec, stop, resume, **delete/delete-own**, snapshot read/write, and provider usage
access. An archive/stop-only key is not sufficient. No vault enrollment is created
by this probe; the operator supplies their existing authorized reader.

### Runner host prerequisites (no installs)

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin --runner-prerequisites \
  --output .cortexkit/internal-docs/research/boat-runner-prerequisites --max-minutes 5
```

Allow about **one minute**, plus cleanup. Creates one `large`, records
`/etc/os-release`, systemd version and an actual `PrivatePIDs=yes` transient
unit, effective SSH policy/configuration, and root open plus
`KVM_GET_API_VERSION` on `/dev/kvm`. Nested-module flags are recorded separately;
an API version alone does not prove a nested guest boots. No VM is created, no
tools are installed, and PID 1 and sshd are never replaced. Unsupported
`PrivatePIDs` is retained as raw evidence, not hidden by dropping the property.
Creation uses `noEnv: true` so Boat attaches no account environment, secrets or
credentials. An account-credential audit enumerates sensitive
environment variable names and credential paths in `/root`, `/home/user` and
`/etc`, without logging their values or contents; scan errors prevent a clean
audit. This is a bounded detection check, not proof that arbitrary hidden secrets
cannot exist. The ordinary cleanup/recovery contract below applies. Prerequisite success means
evidence was collected, not that all runner capabilities passed.

### SSH reachability from an IPv4-only Mac

`--ssh-reach` is a separate focused mode; it also requires local `ssh` and
`ssh-keygen`, and the Boat key's `ssh` action:

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin --ssh-reach \
  --output .cortexkit/internal-docs/research/boat-ssh --max-minutes 10
```

It creates one fresh `large`, generates throwaway Ed25519 tested/witness keys in
an owner-only temporary directory, and authorizes only their public keys through
`POST /sandboxes/{id}/sshkey`. It records `sshEndpoint` versus `machineIp`, prefers
the returned endpoint, and forces an IPv4 SSH connection. Known-hosts contains
only the API-returned host key. `StrictHostKeyChecking=yes`, a private known-hosts
file and alias, no global known-hosts, no DNS trust, no updates, no agent, no
password fallback and no multiplexing prevent TOFU or an old session from masking
a failure.

The mode separately measures an SSH command and a **loopback-only** TLS echo
service reached through `ssh -N -L` to `127.0.0.1:9443`. It does not open 9443 in
UFW or expose the TLS service publicly. It stops/resumes twice, records host-key
and endpoint changes, tests the old private key before reauthorizing that tested
key, then writes exactly
`restrict,port-forwarding,permitopen="127.0.0.1:9443"` on its authorized_keys line.
It tests command refusal, shell refusal and allowed forwarding independently,
then resumes and tests the same restriction again.

The shell test requests a shell without a PTY: refusal to allocate a PTY alone
does not establish shell refusal. Restriction persistence compares the exact
tested line and reports normalization/whitespace changes as FAIL with the raw
before/after lines for inspection.

A different throwaway witness public key obtains the refreshed host key after
resume. Reinstalling the tested key before checking persistence could normalize
its options and invalidate the test, so that never happens during the restricted
resume check. The script preserves all unrelated authorized_keys lines and
collapses duplicate lines only for its own tested key. The private keys are never
logged and are removed locally; each local SSH tunnel is terminated and reaped.

Every check has PASS/FAIL plus raw evidence under `ssh_reach`. An authentication
or route failure does not count as successful shell refusal. OpenSSH's `restrict`
option does not itself forbid command execution; the mode measures the requested
combination rather than assuming it creates a forwarding-only login or silently
adding a forced command. Negative capability findings are FAIL, not a rewritten
expectation. Overall exit also remains nonzero while final data purge is pending.
Allow roughly **3–8 large-sandbox minutes**, plus asynchronous purge latency; this
is an estimate, not a measured SSH result. No toolchain or named snapshot is
created in this mode. Boat SSH reference: <https://docs.boat.dev/ssh-access>.

The output directory must not exist. It contains an owner-only `results.json`
checkpoint and a rendered `results.md`; the same JSON is printed to stdout after
cleanup. Keep evidence under ignored `.cortexkit/internal-docs/`, outside build
directories. The Markdown template is embedded in `probe.py` (`TEMPLATE`) so the
renderer works without private task documents. Re-render without a credential:

```sh
python3 script/boat-probe/probe.py \
  --render .cortexkit/internal-docs/research/boat-live/results.json
# Export the unfilled Markdown template instead:
python3 script/boat-probe/probe.py \
  --write-template .cortexkit/internal-docs/research/boat-findings-template.md
```

## What it measures

- One fresh `large` (provider-reported 8 vCPU, 16 GB), no inherited account
  environment/logins (`noEnv: true`). Records raw uid, sudo, process capabilities,
  kernel, PID 1, systemctl, transient systemd unit, cgroup controllers and child
  cgroup writes; each namespace separately with noninteractive sudo if available;
  KVM presence, filesystems, capacity and a real Linux FICLONE ioctl.
- A TLS echo listener on TCP 9443 using a guest-generated certificate trusted by
  the outside client, not Boat's HTTPS proxy or SSH endpoint. Tests the public IP
  before and after the documented guest `sudo ufw allow 9443/tcp`, with raw
  `iptables`/`ip6tables` rule dumps. Only this port is opened; Boat's control ports
  and outbound policy are not modified. Records DNS
  and direct outbound HTTPS to example.com. IPv6-only sandboxes require IPv6 on
  the operator machine; an unreachable client route is not firewall evidence.
- Three complete stop/resume cycles, provider-state timing, command-ready timing,
  median, IP pairs, file-marker hashes, loopback listener readiness, firewall dumps
  and external TLS echo **before** reapplying each resume's rules.
- Three real Rust default-profile toolchains (1.85.1, 1.86.0, 1.87.0) in the
  snapshot-persisted home directory; allocated bytes and install/version outputs.
  The result flags whether at least 2 GiB really installed; no sparse-file padding
  substitutes for installed tools. This deliberately downloads rustup and official
  Rust distributions in a disposable guest, not a production image build recipe.
- Named snapshot save through ready, stop original, create a second `large` from
  that snapshot, check marker/toolchain availability, then one loaded stop/resume.
  Ready timings do not imply all files hydrated; toolchain-ready timings execute
  the installed compilers after waiting for full hydration. Marker checks also
  wait for hydration, rather than treating a file still restoring as data loss.
  No motor-runner or broker is installed by this probe;
  it does not establish runner token enforcement or end-to-end job correctness.
- Provider usage via `GET /sandboxes/{id}/usage` before deletion. Usage is never
  inferred from guest clocks. Raw API responses and timings are retained.

Allow roughly **15–30 large-sandbox minutes**, plus a few minutes of cleanup;
installation and snapshot speed are unmeasured until the operator runs it. The
original stops before the clone starts, rather than billing both in parallel.
`--max-minutes` limits the measurement wall-clock budget, not provider billing or
cleanup duration. API calls have 40-second timeouts. An 1800-second auto-stop TTL
is a backstop, not deletion (Boat may extend TTL while work is active). Actual
charges come from Boat's returned usage; named-snapshot storage can be separately
charged if the account is above its free allowance.

Guest commands are detached: Boat gives them no server-side time limit. The
poll budget is client-side; on timeout the harness enters resource deletion.
Individual guest subprocesses also have bounded timeouts. Command output is
limited to the API's maximum 8 MiB tail; truncation fails visibly instead of
silently parsing incomplete measurements.

## Cleanup and interruption recovery

`finally` handles errors, Ctrl-C and SIGTERM and attempts **every** resource's
deletion. A second signal is ignored during cleanup; SIGKILL, host loss or a
revoked/insufficient key still requires recovery. IDs are checkpointed before
probes. A lost create response is recovered using the same documented
`Idempotency-Key` and request body (valid for 24 hours; the script refuses replay
after 23 hours). Snapshot names are unique and checkpointed before saving.

Sandbox deletion sends the required `X-Ascii-Confirm-Delete` header and polls the
returned deletion operation. A 202 or an archived/hidden sandbox is **not** marked
fully purged. Pending/blocked deletion, missing delete scope or a snapshot save
still in flight is printed as incomplete and exits nonzero. A deletion operation
at `waiting_for_uploads` means deletion was authorized and accepted, but stored
data awaits expiry of existing upload URLs (`expectedBy`); a different key scope
does not accelerate it. The harness reports this state without several minutes
of redundant polling. It attempts fallback stop only when no deletion operation
was accepted. Retry after the operation's reported `expectedBy`, or immediately
after correcting an actual permission error:

```sh
<key reader> | python3 script/boat-probe/probe.py --key-stdin \
  --cleanup .cortexkit/internal-docs/research/boat-live/results.json
```

Recovery operates only on the checkpoint's recorded IDs/names. Treat checkpoint
files as trusted input, not files obtained from an untrusted sandbox. A completed
cleanup reports each resource as deleted and `cleanup_complete: true`.

`Error: none recorded` in rendered findings means no measurement exception was
caught. It does not override `cleanup_complete: false` or a failed network probe.
Older output used `Error: "unmeasured"` for the same absent JSON `error` field;
that was a rendering placeholder, not a provider error or an unrun measurement.

## Portable verification

```sh
python3 --version
python3 -m unittest -v script/boat-probe/test_probe.py
python3 scripts/run-script-tests.py
```

API shapes follow [v1](https://docs.boat.dev/api/v1), including the linked stop,
resume, named-snapshot, sandbox-usage and irreversible-delete references. Raw
networking follows [Public URLs / raw TCP](https://docs.boat.dev/hosting) and
[Firewall](https://docs.boat.dev/hosting/firewall): guest root rules, not an
invented provider firewall API. No live provider assertion follows from these
portable tests.
