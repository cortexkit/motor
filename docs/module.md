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
`ck motor run` (`ck-motor run`) submits one job from a script, and
`ck motor attach JOB_ID` reconnects to it; see
[Direct runs](#direct-runs).

## Configuration

The config file is JSONC. Unknown keys are refused. All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `runner_instance` | `main` | Runner instance for Linux jobs. |
| `windows_runner_instance`, `windows_runner_state_root` | absent | Legacy Windows runner instance and absolute state root. Set both or neither; the instance must differ from `runner_instance`. Used when `windows_runners` is empty. |
| `windows_runners` | `[]` (omitted) | Ordered Windows endpoints: `name`, `host`, `port` (22), `user`, pinned OpenSSH `host_key`, `instance`, `state_root`, `max_concurrent` (1). Takes precedence over the legacy pair. See [windows-jobs.md](windows-jobs.md). |
| `worktree_roots` | `$XDG_DATA_HOME/cortexkit/alfonso/worktrees` | Absolute roots under which linked worktrees are recognised as task workspaces. |
| `queue_wait_limit_s` | `300` | Default queue-wait budget for a job. |
| `hosted_repositories` | `[]` (omitted) | Absolute main-checkout repository roots whose ordinary Linux jobs request hosted leases instead of the direct runner. See [repository routing](#hosted-repository-routing). |
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
| A direct local caller, when `admit_direct_exec` is on | `exec.run`, and `exec.attach`, `exec.cancel`, `exec.status` for jobs its route started or whose secret it holds |
| Any other reserved, direct, unverified or absent principal | Nothing; refused by name |

Tests (`crates/motor-module/tests/it/placement.rs`): `aft_bind_is_admitted`,
`direct_bind_is_refused_by_name_even_when_harness_claims_aft`,
`unverified_and_absent_principals_are_refused`,
`core_bind_and_workspace_prepare_are_admitted`, `core_is_refused_exec_run`,
`aft_is_refused_workspace_prepare`,
`health_flow_reaches_only_the_read_only_health_report`,
`other_reserved_principals_are_refused_at_bind`.

The daemon's `health.check` reads an in-memory build-server sample, refreshed in
the background every 15 seconds with a ten-second timeout; it never connects to
the runner itself. Metrics include host capacity, hourly queue wait, and the top
five repositories by reserved vCPU-hours over 24 hours. A failed sample makes
runner fields null; three consecutive failures or two minutes without a
successful sample degrade module health. Busy queues alone remain healthy. The
`history` metric contains at most 180 oldest-first minute points with the keys
`minute_ms`, `permits_held`, `permits_cap`, `queue_depth`, and `queue_p90_ms`;
only minutes with a successful sample appear. Startup and credential failures
still report failing health, and the detail still includes diagnostics for an
unreachable runner. Tests:
`health_check_reads_only_cache_without_runner_calls`,
`busy_server_is_healthy_and_metrics_have_cli_shape`,
`idle_cache_stales_after_two_minutes_and_recovers`,
`unreachable_samples_are_null_and_three_failures_degrade`,
`history_keeps_the_last_sample_in_each_minute`,
`history_is_bounded_to_180_points_and_evicts_the_oldest`,
`history_leaves_a_gap_for_a_minute_without_a_successful_sample`.

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

That directory holds an exclusively locked `lease` file, a compatible
`state.json` checkpoint (published atomically through `state.tmp`), checksummed
`journal-<epoch>-<sequence>.jsonl` segments, transient `spools/`, and the agent's
runtime directories. Its Engram catalog entry remains a **device-local filetree**;
copying or restoring it to another device can replay stale intents.

With the journal backend, each synchronous mutation appends the complete changed
records and deleted keys
as one SHA-256-checked transaction and fsyncs it before returning. Cancellation,
Run bindings, resubmission bindings, pending broker finishes, and direct-secret
creation are not deferred. Legacy fallback synchronously writes JSON snapshots.
A partial or checksum-invalid final journal frame is
ignored and truncated before the next append; interior corruption or sequence
gaps fail startup rather than silently dropping durable intents. Tests:
`journal_replays_atomic_upserts_and_deletions_without_shutdown`,
`torn_final_frames_are_truncated_before_later_appends`,
`corrupt_interior_frames_and_sequence_gaps_are_errors`.

A background worker rotates the journal and publishes a checkpoint after 1 MiB
or when a non-empty journal's checkpoint reaches five minutes. It retains covered
segments until the checkpoint rename and directory sync succeed. At 8 MiB
(plus at most one transaction) writers wait for compaction, or fail before their
effect if compaction failed. Failures are logged; the five-minute freshness bound
cannot be guaranteed while checkpointing is failing. Tests:
`checkpoint_rotation_publication_and_cleanup_keep_the_suffix`,
`checkpoint_age_triggers_at_five_minutes_even_below_byte_threshold`,
`hard_budget_rejects_effects_when_background_compaction_fails`.

Opening a metadata-free legacy JSON store migrates it once under the same lease.
Migration failure logs a warning and leaves the synchronous JSON writer usable;
errors replaying an already-active journal do not fall back to a stale checkpoint.
The initial implementation still clones and compares the in-memory state; the
record-sized improvement is in disk writes, not CPU complexity. The test
`lifecycle_writes_record_sized_bytes_with_8000_retained_jobs` measures actual file
payload bytes for the same eight-transition hosted lifecycle with 8,000 retained
jobs and 547 snapshots, reporting compaction bytes separately.

The effective checkpoint plus journal holds:

- `jobs`, keyed by job ID: the queue-wait limit, any pending cancellation, the
  terminal record when `ck-motor` itself ended the job (for example a refusal
  before upload), optional attribution (session, preset, scope), and an optional
  `direct_secret_hash` (SHA-256 of a direct job's secret, never the secret itself);
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

The legacy record fields remain compatible. The optional `_journal` checkpoint
metadata identifies format version 1, epoch, sequence and checkpoint time; old
readers ignore it. Absent attribution loads as none. Test:
`older_state_loads_without_attribution_and_keeps_cancel_intent`.

### SUBC rollback procedure

SUBC normally sends SIGTERM, waits for ck-motor to exit, and only then places the
previous binary. SIGTERM, GOODBYE/EOF and orderly shutdown freeze durable
mutations, join compaction and publish a current compatible `state.json` **before
releasing the storage lease**. Export failures are logged as errors: do not
proceed with rollback on such an error.

After a crash, or if shutdown export failed, use the **new, journal-aware binary**
while the daemon is stopped:

```sh
ck-motor store-checkpoint --root /path/to/store.db.ck-motor
```

This offline command acquires the same exclusive lease, replays the journal,
exports current compatible JSON and exits non-zero on any export/replay/lease
failure. Only replace the binary after exit status 0. Do not delete journal files
or copy state.json by hand. The previous `222d833` binary cannot replay a journal.
An abrupt stop followed by rollback **without** this command may lose up to five
minutes of transitions, including cancellation and Run intent fences (longer if
checkpointing was failing). A clean stop is lossless only if its checkpoint
export succeeds: check shutdown logs for export errors rather than treating exit
alone as proof. After a crash or failed export, require the offline command's
exit status 0.
On re-upgrade, metadata removed by an old writer makes its JSON authoritative;
stale journal epochs are not replayed over that writer's changes.

`python3 scripts/mixed-version.py` retains the oldest-client wire lane and builds
the unchanged `222d833` module for rollback. Tests
`rollback_clean_shutdown_exports_every_transition_for_222d833` and
`rollback_offline_checkpoint_exports_crash_suffix_for_222d833` compare all job fields except the renewed `retain_until_ms` through an actual old
reader/writer, including `pending_cancel`, `run_sent`,
secret hashes, pending finishes, snapshots and verified tips. The offline command's
lease and failure behavior is covered by
`offline_checkpoint_takes_the_lease_and_fails_closed_on_export_error`.

The optional direct-secret hash defaults to absent and is omitted for other
jobs. It expires with the retained job record, not when a route disconnects.
The ck-motor `state.json` reader at revision `7274b0b` ignores the new hash
field and can load a store written by this version;
if that older module rewrites the store, it drops the hash, so upgrading again
does not recover reattachment to those jobs. Tests:
`direct_secret_hash_survives_reopen_and_expires_with_its_job`,
`direct_secret_reattaches_replays_and_never_leaks`.

On shutdown `ck-motor` exports its storage checkpoint, stops its SSH control
master before stopping the agent, and removes its runtime directory. A force-kill
skips that cleanup: stop any
orphaned SSH master before reusing a crashed installation's storage root.

## Direct runs

`ck motor run [--vcpus N] [--sibling PATH]... [--env NAME=VALUE]... [--timeout SECONDS] [--cwd PATH] [--secret-file PATH] -- COMMAND...`
runs one command on the runner from a script with no agent behind it, streams
its output, and exits with its status. It works only when `admit_direct_exec`
is `true` in the configuration file ([Configuration](#configuration)).

The words after `--` follow two rules:

- **One word** is passed verbatim as the runner's `bash -c` command string.
- **Two or more words** are an argv. Each word is POSIX-single-quoted (with
  each `'` escaped as `'\''`) and joined with spaces, except that nonempty
  words consisting only of `[A-Za-z0-9_@%+=:,./-]` are left unquoted for
  readable job records and logs. Bash therefore preserves each argument.

Quote a whole shell command as one word, or pass it as one argument to `sh -c`:

```sh
ck motor run -- 'cargo test -p x 2>&1 | grep FAILED'
ck motor run -- sh -c 'cargo test -p x 2>&1 | grep FAILED'
```

Tests: `command_line_one_word_is_verbatim`,
`command_line_plain_words_stay_readable`,
`command_line_sh_c_round_trips_through_bash`,
`command_line_special_word_round_trips_through_bash`,
`command_line_empty_word_round_trips_through_bash`.

| Exit status | Meaning |
| --- | --- |
| The command's own code | The command exited. |
| 128 + N | Signal N killed the command. |
| 200 | Run refused before start (see the printed reason), or attach refused (unknown job or invalid secret). |
| 201 | Outcome unknown, or the connection was lost. The job ID is printed. |
| 202 | The runner stopped the command at its timeout. |
| 203 | The job was cancelled. |
| 204 | Bad arguments; nothing was submitted. |

Every direct run has a timeout (default 3600 seconds), because a disconnected
caller may never reconnect. Direct runs get
a 30-minute queue-wait budget and, when the runner supports it, run ahead of
agent jobs; see [scheduling.md](scheduling.md). Source:
`crates/motor-module/src/cli.rs`.

`--hosted` sends that one job straight to a hosted lease instead of the build
server, as the operator's test of the hosted path; without a configured broker
it exits 204 and submits nothing. See
[hosted-cutover.md](hosted-cutover.md#smoke-test-the-hosted-path).

### Reattaching after a disconnect

At submission `ck-motor` generates 32 random bytes with the OS CSPRNG, encoded
as 64 lowercase hex characters. `run` prints one stderr line with the job ID
and secret. Keep the secret private: possession grants attach, status and cancel for
that job, not submission or access to any other job. For scripts, prefer a new
file outside the checkout and captured command logs:

```sh
ck motor run --secret-file /private/job.key -- 'cargo test'
ck motor attach JOB_ID --secret-file /private/job.key
ck motor attach JOB_ID --secret-file /private/job.key --status
```

`--secret-file` on `run` creates a new file with mode `0600`, refuses an existing
file (including a symlink), and prints its path instead of the secret. Without
`--secret-file`, `attach` reads one line from stdin. Never place the secret in
argv or an environment variable; neither is an accepted input channel. Do not
enable shell tracing while handling it. Delete the file when no longer needed.

`attach` replays output from the start in order, then follows a still-running
job to its terminal outcome. It has the same exit-code contract as `run`.
`--status` instead prints a JSON state/outcome reply without streaming, exiting
0 while running or with the terminal exit code when complete. Ctrl-C or SIGTERM
requests cancellation and waits for the terminal (203); a second interrupt
stops waiting (201). A lost connection does not cancel the job.

The route that submitted the job can still attach, check and cancel without a secret. A new
direct route sends `direct_secret` beside `op` and `params` in ck-motor's JSON
envelope. The first live direct submission record alone carries a `direct_job`
object with `job_id` and `secret`. This is not a shared exec type or a runner
frame: it is added after persistence, never replayed, logged, returned by
status, or sent to AFT or Basal. The module hashes the presented secret and
compares fixed-size digests in constant time before contacting the runner.
Wrong, missing and malformed secrets receive the same refusal bytes and exit
code as an unknown job ID; refused credentials do not cause runner I/O. AFT and
Basal admission and operations are unchanged.

The following tests cover replay after a module restart, live streaming,
secret non-disclosure, per-job authorization, cancellation, status and CLI
refusal equivalence (`crates/motor-module/tests/it/operator_cli.rs`):
`direct_secret_reattaches_replays_and_never_leaks`,
`direct_secret_attach_streams_a_running_job_and_original_route_needs_no_secret`,
`attached_cli_cancels_with_secret_and_status_is_read_only`,
`direct_secret_grants_only_its_job_and_no_additional_operations`,
`cli_attach_refusals_are_byte_identical_and_secret_file_hides_the_secret`.
CLI unit tests: `secret_file_is_private_new_and_never_overwrites`,
`attach_never_accepts_an_argv_secret_or_echoes_it`.

## Hosted repository routing

`hosted_repositories` selects repositories for a staged hosted rollout without
changing other fleet traffic. Matching uses the resolved `repository_root`, not
the workspace key: the main checkout, linked worker worktrees and linked gate
checkouts share that identity. Independent clones have different identities and
must be listed separately. Paths are canonicalized for comparison; entries must
be absolute. The empty default is omitted from serialized Config so older clients
with closed schemas can still read it.

Both agent and direct jobs for a listed repository request a lease before any
direct-runner submission. The lease sends `avoid_host: null` (ignoring
`spill_avoid_host`) and `allow_spill: false`, restricting placement to registered
hosts rather than Boat. These jobs never enter the spill path or fall back to the
direct runner. Windows stays on its direct runner. Outbound-network jobs can use
hosted leases: the request's `net` flag must match the signed lease claim, and
`network_routing` must still be enabled. Unlisted jobs retain their existing
direct and spill behavior; spilled leases and shadow leases send `allow_spill:
true`.

For a repository-routed job, broker `503 no_capacity` responses wait for
`Retry-After` (seconds, clamped to 1–30 seconds; missing or invalid means one
second) before retrying the identical request, including its job ID. Capacity
and `202 provisioning` retries share one queue-wait deadline. Exhaustion yields
a typed retryable refusal, never local execution. Before the durable `run_sent`
marker, refusals identify repository-selected routing with the exact detail text
`hosted stage-1 routing (hosted_repositories)`.
After that marker the existing hosted reattach/finish path governs an ambiguous
submission. Each selected job logs one INFO event, `routed to hosted
(hosted_repositories)`, with `job_id` and `repo` fields. Hosted health metrics
include `hosted_routed_jobs_1h`, counting repository-selected jobs (including
refusals) in the last hour; stored route decisions survive restart.

Tests: `listed_main_and_linked_agent_and_direct_jobs_use_hosted_without_avoiding_a_host`,
`unlisted_and_windows_jobs_keep_the_direct_path`,
`stage1_pre_run_failures_are_typed_and_never_submit_directly`,
`stage1_lost_run_ack_reattaches_without_direct_fallback`,
`stage1_no_capacity_retries_the_identical_body_then_runs_on_the_host`,
`stage1_no_capacity_and_provisioning_share_one_bounded_queue_deadline`,
`unlisted_spill_eligible_job_still_withdraws_and_sends_allow_spill_true`,
`shadow_lease_explicitly_allows_spill`, and
`hosted_routed_counter_only_counts_repository_routes_in_the_last_hour`.
`python3 scripts/mixed-version.py` checks the default Config against the pinned
older client.

## Hosted shadow comparisons

`shadow_to_hosted` is independent of `spill_to_hosted` and defaults to
`{"enabled":false,"sample_rate":0.1,"max_concurrent":2}`. The entire default
object is omitted when serializing configuration, so older clients still load
it. Set `spill_avoid_host` to the build server's broker host ID before enabling
shadowing; without it, selection fails closed.

After build-server acceptance, sampled offline Linux jobs of at most 8 vCPUs
can run a second copy on Boat. Direct-caller jobs (which have reattach secrets),
return-path requests, Windows jobs and network requests are excluded. The
broker must grant the account shadow permission and enable its shadow pool;
`shadow_not_allowed` and `no_capacity` skip the shadow, never reroute the caller.
Shadows use the captured snapshot and filtered environment, never attach, and
never wait on the caller's delivery path. The concurrency bound covers a shadow
and its pending comparison, so a slow primary cannot create unbounded captures.

Completed comparisons append to `shadow.jsonl` in the module's store directory,
rotated at 4 MiB with one previous file. No shadow job is persisted in
`state.json`. Each side records outcome, exit code, wall time and raw-byte
stdout/stderr SHA-256 and byte counts. Only mismatches retain the last 16 KiB of
each stream, displayed as lossy UTF-8; files are created private (0600).
Mismatch classes distinguish refusal before start, outcome kind, exit code and
output differences. Output can legitimately differ because of timestamps and
runner paths, so a mismatch is a triage signal, not a caller failure.

`ck health ck-motor --verbose` includes `shadow` counts for retained comparisons
from the last hour. `ck motor shadow-report [--since UNIX_SECONDS|Nh|Nm|Ns]`
prints retained mismatches with both sides' stdout/stderr tails; like the other
local motor commands, it needs `admit_direct_exec`. Shutdown cancels in-flight
shadows and attempts broker release/finish within a bounded deadline. An abrupt
process kill cannot perform cleanup; the broker's timeout remains the backstop.
No shadow is replayed after restart.

The shadow contracts are exercised by `shadow_hanging_or_refused_never_changes_callers_stream_or_exit`,
`shadow_eligibility_excludes_net_windows_oversize_secrets_returns_and_unknown_host`,
`shadow_concurrency_cap_and_zero_sampling_are_nonblocking`,
`shadow_reuses_pinned_boat_path_and_records_both_tails`, and
`restart_cancels_running_shadow_finishes_lease_and_never_resumes_it`.
