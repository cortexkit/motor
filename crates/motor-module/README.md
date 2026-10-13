# ck-motor placement

`ck-motor` handles `exec-remote/v1` execution and workspace requests routed by the
CortexKit daemon (SubC), snapshots workspaces, and sends commands to `motor-runner`
over SSH. Install it as a **reserved module**, not a shell-accessible provider.
The daemon and its callers are
external integrations; they do not ship in this repository.

## Placement card

- Module ID: `ck-motor`; `reserved: true`.
- Program: the installed `ck-motor` binary built from
  `crates/motor-module/src/main.rs` (`target/release/ck-motor` after
  `cargo build --release -p motor-module --bin ck-motor`). The placement program
  path must be the absolute installed path on the Mac, not a worker checkout.
- Args: `--subc <daemon-connection-file>`, optionally `--config <JSONC-file>`.
- Env: daemon-supplied `SUBC_MODULE_ID=ck-motor` and launch-nonce descriptor;
  ordinary `PATH` (with `ssh`, `git`) and `HOME`.
  `CK_MOTOR_CONFIG` is an optional alternative to `--config`; `XDG_DATA_HOME`
  controls the development storage fallback. **No runner secret in env.**
- Manifest `capabilities.provides` is exactly `["exec-remote/v1"]`.
  It does not provide `flow-scopes/v1` or own/sync scopes.
- Management ops: `exec.run`, `exec.attach`, `exec.cancel`, `exec.status`,
  `workspace.prepare`, `workspace.drop`.

## Integration identities

AFT is the agent-tool caller; the CortexKit core manages workspace lifetime.
Their daemon-verified `reserved:aft` and `reserved:prefrontal-core` principals
bind, with separate per-operation authorization: AFT may call only `exec.run`,
`exec.attach`, `exec.cancel`, and `exec.status`; core calls only
`workspace.prepare` and `workspace.drop` on workspace launch/reclaim. Other
reserved, `direct`, `unverified`, and absent principals have named typed
refusals, regardless of their claimed harness identity. A management-surface
bind itself contains no operation selector and grants no cross-principal ops.

Basal is a scheduled-flow caller: only its authorized health-flow scope may
read motor health, not execute jobs. Claustrum is the credential-vault service
motor calls for public-key lookup and signing; it is not an execution caller.

For scoped `exec.run` requests motor stores the bound `identity.session`, scope
ref/epoch and optional top-level envelope `preset` with the job. The pinned scope
stamp has no session/preset attributes: AFT supplies `preset` for attribution,
and motor never validates or chooses presets. Missing preset is recorded as
`null`; unscoped jobs have no attribution. Attach/cancel do not rewrite the
original attribution. The shared exec-remote caller types are unchanged.

## Credential enrollment

Use Claustrum's credential-management tooling to mint the Ed25519 signing credential
**`signing:ck-motor-ssh:1`** in
the vault, and grant **`reserved:ck-motor` `sign` and `read`** on that credential
(or the `exec-remote-runner` category if the operator deliberately wants a broader
selector). These are Claustrum grant-table enrollments, not authority bestowed
by `manifest.bindings.vault_grants`. Motor never fetches a private-key payload or
uses a raw bearer handle. Its shared credential client opens the Claustrum route
with `consumer_identity {module_id:"ck-motor",launch_nonce:<daemon nonce>}` and
addresses `credential.public_key` and `credential.sign` by `credential_id`.
Signing sends the SSH challenge as `payload_b64`; only the vault holds the key.
Neither key bytes, challenges nor signatures are logged.

For a new enrollment, retrieve the enrolled credential's public key through
authenticated Claustrum access, not from a job workspace or an unauthenticated
message. Encode the 32-byte public key as an OpenSSH `ssh-ed25519` identity and
install that **public** identity in the runner's `authorized_keys`. Do not export
or copy the private key. Key rotation requires updating runner authorization and
restarting motor; an unexpected signing key ID is refused.

Motor serves an in-process SSH agent in a newly created 0700 `a<random-suffix>/`
runtime directory under its storage root. `REQUEST_IDENTITIES` returns the vault
public key; `SIGN_REQUEST` accepts only that identity with Ed25519 flags and
delegates to `credential.sign`. Only `identity.pub` (no comment), agent socket `s`
and master control socket `m` can live in this directory. The public file is
necessary because `IdentitiesOnly=yes` selects agent keys by their public
IdentityFile. No `ssh-agent` or `ssh-add` child ever receives private material.

Normal shutdown, daemon EOF, wire errors and SIGTERM/SIGINT kill/reap motor's
foreground SSH master **before** stopping the agent and removing the runtime
directory. Dropping the last connector also kills the owned master and requests
agent cleanup. A force-kill cannot run Rust cleanup: stop any orphaned SSH master
before reclaiming a crashed installation's storage root. Startup always uses a
fresh runtime directory and never adopts a previous master's authentication.

SSH transport uses `IdentityAgent=<owned-socket>`, `IdentitiesOnly=yes`, the
public-only `-i` file, `BatchMode=yes`, `ForwardAgent=no` and multiplexing with
`ControlMaster=auto`, `ControlPath=<runtime>/m`, `ControlPersist=no`. Motor starts
one owned foreground master lazily and serializes its creation; subsequent runs
use the same authenticated connection without another vault sign. Startup reads
only HostName/User/Port from `ssh -G alfonso-runner`. Configure that SSH alias
with the installation's host, user, and port; the alias name is fixed by this
build. Connections then use `-F /dev/null`, ignoring ambient private
IdentityFiles/agents. Public known-hosts files still apply.
Aliases requiring ProxyJump/ProxyCommand are not supported. Use a short storage
root: the socket's absolute path must fit the Unix socket limit (103 bytes on
macOS); the control master additionally needs room for OpenSSH's temporary socket
suffix (17 bytes). Overlong paths fail closed during agent/master startup.

Missing/refused/unavailable credentials produce failing health with respectively
`runner_credential_missing`, `runner_credential_grant_refused`, or
`runner_credential_unavailable`. Agent startup errors use `runner_agent_start_failed`;
alias-resolution errors use `runner_ssh_config_invalid`; a stopped agent uses
`runner_agent_stopped`. A refused sign returns SSH agent failure and latches failing
health: `runner_sign_missing`, `runner_sign_grant_refused`,
`runner_sign_invalid_response` or `runner_sign_unavailable`. `exec.run` returns a
terminal `refused_before_start {reason}` with `ran:none` **before snapshot or connection**,
using the protocol's additive refusal-reason support so AFT can fall back locally.
Startup public-key lookup and agent-start failures retry in the background with
exponential backoff from 1 second to a 30-second cap; repairing enrollment or a
grant does not require restarting motor, even after `not_found` or `auth_required`
public-key errors. SSH alias-resolution failures are not in that retry loop.
Signing failures remain latched and require restart after
repair. `ssh_config_override` remains available only for non-main test instances
and can select fixture identities without Claustrum.

## Store and evolution

The manifest requests project-scoped SQLite module storage with `owns_schema:true`,
but motor **does not open SQLite or run SQL migrations**. It uses the delivered
SQLite descriptor's path to locate a sibling directory `<path>.ck-motor/`. Without
a descriptor its development fallback is
`$XDG_DATA_HOME/cortexkit/ck-motor/<runner_instance>` (or the HOME data default).

That directory contains an exclusively locked `lease`, atomic/fsynced
`state.json` (written via `state.tmp`), transient `spools/`, and the private agent
runtime directories. JSON state contains:

- `jobs`, keyed by UUID: `queue_wait_limit_s`, `pending_cancel`, `local_terminal`,
  and optional `attribution {session,preset,scope_ref,scope_epoch}`;
- `snapshots`, keyed by workspace: repository-path to accepted-commit maps.

There is no schema/version ledger or formal migration runner. State
decodes with absent attribution defaulting to `None`; unknown additive fields
are ignored by serde. Cancellation intent and terminals retain their existing
durability. An incompatible state change needs an explicit migration strategy.
