# The ck-motor module

`ck-motor` is motor's client. It runs on the caller's Mac as a reserved module
of the CortexKit daemon (SubC). It receives `exec-remote/v1` requests, captures a
snapshot of the caller's checkout, and sends it with the command to
`motor-runner` over SSH. This document covers how it is placed, who may call it,
how it authenticates to the runner, and what it stores.
[architecture.md](architecture.md) shows where it sits in the whole system;
[snapshot-contents.md](snapshot-contents.md) defines what it captures.

The daemon, its callers and the credential vault are separate CortexKit
components. They do not ship in this repository.

## Placement

- Module ID `ck-motor`, installed as a reserved module, not a shell-accessible
  provider.
- Program: the absolute installed path of the `ck-motor` binary, built with
  `cargo build --release -p motor-module --bin ck-motor --locked`. Do not point
  the placement at a binary inside a development checkout.
- Arguments: `--subc <daemon-connection-file>`, and optionally
  `--config <JSONC file>`. `CK_MOTOR_CONFIG` names the config file when
  `--config` is absent.
- Environment: the daemon supplies `SUBC_MODULE_ID=ck-motor` and the launch
  nonce. `PATH` must reach `ssh` and `git`. `HOME` and `XDG_DATA_HOME` locate the
  development storage fallback. No runner secret is passed in the environment.
- The manifest's `capabilities.provides` is exactly `["exec-remote/v1"]`.
- Operations: `exec.run`, `exec.attach`, `exec.cancel`, `exec.status`,
  `workspace.prepare`, `workspace.drop`, and the read-only `runner.health` report.

`ck-motor --ck-domain` prints one line for the `ck` launcher, and
`ck motor run` (`ck-motor run`) submits one job from a script; see
[Direct runs](#direct-runs).

## Configuration

The config file is JSONC. Unknown keys are refused. All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `runner_instance` | `main` | Runner instance for Linux jobs. |
| `windows_runner_instance`, `windows_runner_state_root` | absent | Runner instance and its absolute state root for Windows jobs. Set both or neither; the instance must differ from `runner_instance`. Without them, Windows requests are refused with `platform_unsupported`. |
| `worktree_roots` | `$XDG_DATA_HOME/cortexkit/alfonso/worktrees` | Absolute roots under which linked worktrees are recognised as task workspaces. |
| `queue_wait_limit_s` | `300` | Default queue-wait budget for a job. |
| `network_routing` | `false` | Forward a caller's request for outbound network. When off, such requests are refused with `network_unsupported`. |
| `admit_direct_exec` | `false` | Admit direct local callers (`ck motor run`) for their own jobs. |
| `direct_exec_max_timeout_secs` | `7200` | Longest timeout a direct run may ask for. |
| `ssh_config_override` | absent | Test control: an SSH config file used instead of the vault-backed identity. |
| `inject_tree_hash_mismatch`, `inject_snapshot_failure` | `false` | Test controls that force a tree-hash mismatch or a snapshot failure. |

The three test controls are refused when `runner_instance` is `main`, so a
production configuration cannot weaken authentication or fake failures.

Source: `crates/motor-module/src/config.rs`.

## Who may call it

The daemon verifies each caller's identity; `ck-motor` authorizes each
operation separately. Binding to the module grants nothing by itself.

| Caller | Allowed operations |
| --- | --- |
| AFT, the agent tool (`reserved:aft`) | `exec.run`, `exec.attach`, `exec.cancel`, `exec.status` |
| The CortexKit core (`reserved:prefrontal-core`) | `workspace.prepare`, `workspace.drop` |
| Basal's health flow | `runner.health` only |
| A direct local caller, when `admit_direct_exec` is on | `exec.run`, and `exec.attach`, `exec.cancel`, `exec.status` for jobs its own route started |
| Any other reserved, direct, unverified or absent principal | Nothing; refused by name |

Tests (`crates/motor-module/tests/it/placement.rs`): `aft_bind_is_admitted`,
`direct_bind_is_refused_by_name_even_when_harness_claims_aft`,
`unverified_and_absent_principals_are_refused`,
`core_bind_and_workspace_prepare_are_admitted`, `core_is_refused_exec_run`,
`aft_is_refused_workspace_prepare`,
`health_flow_reaches_only_the_read_only_health_report`,
`other_reserved_principals_are_refused_at_bind`.

For scoped `exec.run` requests, `ck-motor` stores the caller's session, scope
reference and epoch, and the optional `preset` with the job, for attribution
only. It never validates or chooses presets. Test:
`scoped_jobs_persist_session_preset_and_scope_without_preset_policy`.

## Authenticating to the runner

`ck-motor` signs in to the runner's SSH account with an Ed25519 key that lives
only in the credential vault (Claustrum). It never holds the private key.

1. Mint the signing credential `signing:ck-motor-ssh:1` in the vault and grant
   `reserved:ck-motor` `sign` and `read` on it with the vault's own tooling.
2. Read the credential's public key through authenticated vault access, encode
   it as an OpenSSH `ssh-ed25519` line, and add it to the runner account's
   `authorized_keys`.
3. Define an SSH alias named `alfonso-runner` (the name is fixed in this build)
   with the runner's `HostName`, `User` and `Port`. `ck-motor` reads only those
   three values from `ssh -G alfonso-runner`. Aliases that need `ProxyJump` or
   `ProxyCommand` are not supported.

At runtime `ck-motor` serves its own SSH agent on a socket in a fresh `0700`
directory under its storage root. The agent offers only the vault's public key
and forwards each signature request to the vault. SSH runs with
`-F /dev/null`, `IdentitiesOnly=yes`, `BatchMode=yes`, `ForwardAgent=no`, and
one multiplexed control master that `ck-motor` owns. Known-hosts files still
apply. Test: `vault_agent_has_no_private_material_on_disk_and_shuts_down`.

Keep the storage root path short: the agent socket's absolute path must fit the
Unix socket limit (103 bytes on macOS), with room for OpenSSH's temporary
control-socket suffix. An overlong path fails at startup.

Rotating the key means updating `authorized_keys` and restarting `ck-motor`; a
signature from an unexpected key ID is refused.

### Credential failures

Health reports these codes, and `exec.run` refuses before taking a snapshot or
opening a connection, so the caller can fall back to running locally. Caller
run and Prepare refusals use the typed `runner_authentication_failed` reason;
`refusal_detail` retains the specific safe code. Health and background credential
recovery continue to distinguish the codes below.

| Code | Cause |
| --- | --- |
| `runner_credential_missing`, `runner_credential_grant_refused`, `runner_credential_unavailable` | The public-key lookup failed. Retried in the background from 1 second, doubling to 30 seconds; fixing the vault does not need a restart. |
| `runner_agent_start_failed` | The agent could not start. Retried the same way. |
| `runner_ssh_config_invalid` | The SSH alias could not be resolved. Not retried. |
| `runner_agent_stopped` | The agent stopped. |
| `runner_sign_missing`, `runner_sign_grant_refused`, `runner_sign_invalid_response`, `runner_sign_unavailable` | A signature was refused. Latched until `ck-motor` restarts. |

Tests: `missing_key_fails_health_and_run_before_snapshot_or_ssh`,
`refused_grant_fails_health_and_run_before_snapshot_or_ssh`,
`missing_enrollment_recovers_health_without_restart`,
`credential_backoff_doubles_caps_and_stops_on_shutdown`,
`agent_start_failure_recovers_without_restart`.

## Storage

The daemon delivers a SQLite storage descriptor, but `ck-motor` never opens
SQLite. It uses the descriptor's path to find a sibling directory
`<path>.ck-motor/`. Without a descriptor it uses
`$XDG_DATA_HOME/cortexkit/ck-motor/<runner_instance>`.

That directory holds an exclusively locked `lease` file, `state.json` (written
atomically through `state.tmp`), transient `spools/`, and the agent's runtime
directories. `state.json` holds:

- `jobs`, keyed by job ID: the queue-wait limit, any pending cancellation, the
  terminal record when `ck-motor` itself ended the job (for example a refusal
  before upload), and optional attribution (session, preset, scope);
- `snapshots`, keyed by workspace: the commits the runner has accepted, used to
  send only missing history next time.
- `verified_tips`, keyed by canonical Git common directory: only the last
  successfully checked origin URL, branch and commit per repository, with its
  verification time. At most 256 repositories survive for at most 24 hours;
  hits do not extend their age. Failed checks are not stored as successes. See
  [compile-cache.md](compile-cache.md#verifying-the-fill-tip) for the credential
  environment, timeout, warning fields and remaining trust in this local store.
  Tests: `successful_verification_survives_storage_reopen`,
  `verified_tips_keep_only_256_repositories_and_one_tip_each`.

Failed tip checks are cached only in memory for 60 seconds, keyed by repository,
effective origin URL, branch and local commit, with at most 256 entries. Hits
return no tip without another remote query or warning, and do not extend the
TTL. A changed key or an expired entry rechecks. These entries never enter
`state.json`; restarting the module forgets them. Tests:
`negative_cache_is_not_persisted`, `negative_cache_rechecks_after_ttl`.

There is no schema version. Unknown fields are ignored and absent attribution
loads as none; an incompatible change needs an explicit migration. Test:
`older_state_loads_without_attribution_and_keeps_cancel_intent`.

On shutdown `ck-motor` stops its SSH control master before stopping the agent
and removing its runtime directory. A force-kill skips that cleanup: stop any
orphaned SSH master before reusing a crashed installation's storage root.

## Direct runs

`ck motor run [--vcpus N] [--sibling PATH]... [--env NAME=VALUE]... [--timeout SECONDS] [--cwd PATH] -- COMMAND...`
runs one command on the runner from a script with no agent behind it, streams
its output, and exits with its status. It works only when `admit_direct_exec`
is `true` in the configuration file ([Configuration](#configuration)).

| Exit status | Meaning |
| --- | --- |
| The command's own code | The command exited. |
| 128 + N | Signal N killed the command. |
| 200 | Refused before start; nothing ran. |
| 201 | Outcome unknown, or the connection was lost. The job ID is printed. |
| 202 | The runner stopped the command at its timeout. |
| 203 | The job was cancelled. |
| 204 | Bad arguments; nothing was submitted. |

Every direct run has a timeout (default 3600 seconds), because a direct caller
that loses its connection cannot reattach to or cancel its job. Direct runs get
a 30-minute queue-wait budget and, when the runner supports it, run ahead of
agent jobs; see [scheduling.md](scheduling.md). Source:
`crates/motor-module/src/cli.rs`.
