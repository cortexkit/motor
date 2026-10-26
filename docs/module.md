# The ck-motor module

`ck-motor` is motor's client. It runs on the caller's machine as a reserved module
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
- The manifest's `capabilities.provides` is `["exec-remote/v1", "flow-scopes/v1",
  "exec-remote-return/v1", "exec-remote-return/v2"]`, plus `exec-remote-windows/v1` when a Windows runner
  is configured and `exec-remote-net/v1` when `network_routing` is on.
- Operations: `exec.run`, `exec.attach`, `exec.cancel`, `exec.status`,
  `workspace.prepare`, `workspace.drop`, and the read-only `runner.health` report.
- Requires SubC daemon **0.36.35 or newer**. The module uses subc-client-rs
  0.32.4 with subc-protocol 0.33.0, subc-transport 0.14.0 and subc-os 0.1.19.
  Install through the ck-motor card placed by SubC; replacing the binary does
  not upgrade an older daemon's protocol support.

`ck-motor --ck-domain` prints one line for the `ck` launcher, and
`ck motor run` (`ck-motor run`) submits one job from a script,
`ck motor attach JOB_ID` reconnects to it, and `ck motor cancel JOB_ID` stops it;
see [Direct runs](#direct-runs).

### Dispatch capacity and route ordering

The SDK has two independent handler pools: 256 long-held slots for `exec.run`
(both agent and direct calls) and `exec.attach`, and 64 default slots for short
operations such as status and cancel. Attach replays records and then follows
the job through its terminal record, so it belongs in the long-held pool too.
The classifier peeks at the compact JSON envelope's `op` or `method` prefix;
the direct CLI's secret-first attach envelope is also recognized. It never
decodes command parameters on the connection's frame reader. AFT serializes
`serde_json::to_vec(&json!({"method": method, "params": params}))`, so its run
and attach operation names precede params, including on keyed resubmissions.
The closing quote and following comma or object end delimit the operation;
names such as `exec.run_x` and `exec.attachment` use the default pool.

The former fleet-wide 56-run admission cap and eight-slot control reservation
are gone. The 256 slots are a dispatch budget, not a job-capacity refusal:
runners and hosted tenants make the actual admission decision. They cover a
96-permit runner plus a 64-slot tenant with room for additional streams,
including attach. Above 256 open streams the SDK waits in the long-held pool,
not the default pool; sustained multiple attaches per job can consume that
headroom. The SDK warns after two seconds waiting and immediately answers a
request cancelled while waiting. Local Windows pre-admission refusals retain
`windows_slots_busy` detail; hosted capacity refusals retain their `hosted_*`
cause. Runner-originated refusals keep the runner's detail.

The management surface declares `stateless_parallel`, allowing 1024 in-flight
requests per route instead of `module_managed`'s 32. **There is no FIFO
submission or completion guarantee within a route.** Job-specific follow-ups
use an explicit job ID; run mints it and registers the job before publishing
acceptance.
Callers learn the ID from the stream, not from submission order. Attach waits
for submission to settle; explicit `exec.cancel` persists cancellation intent
even while the runner is admitting the job. Workspace prepare/drop sequencing
is the caller's responsibility: await prepare before a dependent run.

AFT, the agent tool, currently uses one connection and one route per remote job
(at most two or three calls in flight). Its live cancellation sends a Cancel
frame on the run subscription and immediately follows with attach, or status
before it knows the job ID, on the same route. That Cancel detaches the stream, not the job;
the follow-up does not depend on executing first or last. Restart cancellation
uses explicit `exec.cancel {job_id}` on a fresh connection. Neither pattern
needs FIFO, and `stateless_parallel` avoids a latent bottleneck for other
callers that multiplex jobs.

`ck health motor` (verbose metrics) and `runner.health` include compact
`request_pools.default` and `request_pools.long_held` objects with `in_use`,
`limit`, `waiting`, and nullable `oldest_wait_ms`. These are live in-memory
snapshots, not runner probes or per-request lists; they add only a few hundred
bytes to the module's bounded health cache (16 KiB maximum). The SDK separately
degrades health when a full pool has waited more than two seconds.

If an authorized run or attach handler sees a long-held operation that the
prefix classifier put in the default pool, it increments
`request_pools.long_held_fallbacks_total` (since module start). This catches
unexpected serialization, such as params appearing before method. A WARN
names the operation and first envelope key, at most once per minute across
the module; every fallback still increments the counter. The key is decoded
only in the handler, bounded to a small envelope head and 64 characters, and
the warning never includes parameter values. A rising counter calls for fixing
the caller's envelope order or extending the classifier, not increasing runner
capacity.

The SDK upgrade does not change motor's handler/HELLO_ACK signatures or bind
authorization. `on_hello_ack` still runs before frame reading starts, so storage
opening remains a background task and storage-dependent requests wait for it.
Unknown bind `call_mode` values are refused as `motor_call_mode_unsupported`;
known modes do not grant authority. Motor continues using the daemon's
principal and scope and does not advertise `call-modes/v1`. The SDK
now treats post-ACK resets as clean connection ends, including while that hook
is pending, and handler panics fail serving rather than silently losing replies.

Tests: `canonical_agent_and_direct_stream_envelopes_use_long_held_pool`,
`eighty_run_streams_leave_status_cancel_and_attach_responsive`, and
`same_route_stream_cancel_and_immediate_followup_do_not_cancel_jobs`.
Bind compatibility: `unknown_call_mode_is_refused_without_breaking_known_binds`.
Envelope and telemetry coverage:
`aft_method_envelopes_and_keyed_resubmits_use_long_held_pool`,
`keyed_aft_resubmits_keep_both_streams_in_long_held_pool`,
`reordered_stream_envelopes_increment_health_counter_and_warn_once`, and
`fallback_counter_counts_every_dispatch_but_warns_once_per_minute`.

## Configuration

The config file is JSONC. Unknown keys are refused. All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `runner_instance` | `main` | Runner instance for Linux jobs. |
| `windows_runner_instance`, `windows_runner_state_root` | absent | Legacy Windows runner instance and absolute state root. Set both or neither; the instance must differ from `runner_instance`. Used when `windows_runners` is empty. |
| `windows_runners` | `[]` (omitted) | Ordered Windows endpoints: `name`, `host`, `port` (22), `user`, pinned OpenSSH `host_key`, `instance`, `state_root` (absolute POSIX or drive-letter path), `max_concurrent` (1). Takes precedence over the legacy pair. See [windows-jobs.md](windows-jobs.md). |
| `worktree_roots` | `$XDG_DATA_HOME/cortexkit/alfonso/worktrees` | Absolute roots under which linked worktrees are recognised as task workspaces. |
| `queue_wait_limit_s` | `300` | Default queue-wait budget for a spill-eligible job; see below for jobs that cannot spill. |
| `hosted_repositories` | `[]` (omitted) | Absolute main-checkout repository roots whose ordinary Linux jobs request hosted leases instead of the direct runner. See [repository routing](#hosted-repository-routing). |
| `hosted_repositories_placement_hint` | absent (omitted) | Optional `{ "host": "control-host", "vm": "t1" }` placement hint for repository-routed leases only. Both names must follow the broker grammar; `vm` cannot be null. Hinted jobs never fall back to the build server. |
| `network_routing` | `false` | Forward a caller's request for outbound network. When off, such requests are refused with `network_unsupported`. |
| `admit_direct_exec` | `false` | Admit direct local callers (`ck motor run`) for their own jobs. |
| `direct_exec_max_timeout_secs` | `7200` | Longest timeout a direct run may ask for. |
| `ssh_config_override` | absent | Test control: an SSH config file used instead of the vault-backed identity. |
| `inject_tree_hash_mismatch`, `inject_snapshot_failure` | `false` | Test controls that force a tree-hash mismatch or a snapshot failure. |

The three test controls are refused when `runner_instance` is `main`, so a
production configuration cannot weaken authentication or fake failures.

An ordinary job's explicit `queue_wait_limit_s` always wins. Without one,
spill-eligible jobs use the configured default (five minutes); jobs that cannot
spill use 30 minutes, capped by the job's execution timeout when present. This
includes net jobs, Windows jobs, jobs above `spill_max_vcpus` (eight by default),
and jobs excluded by any other spill eligibility or routing rule. The longer
budget lets them retain their place in the build-server queue rather than
rejoining at the back after a refusal. Gate jobs keep their fixed 30-minute
budget, regardless of the request's queue limit or execution timeout. The
resolved budget is stored per job in the checkpoint/journal and reused after a
module restart.

If a spill fails with `hosted_capacity` or `hosted_provisioning` before a hosted
runner receives the job, an ordinary job using the default queue budget returns
to the build server with the non-spill budget: 30 minutes from its original
submission, capped by its execution timeout. Time already spent queuing and
trying hosted capacity is deducted, rather than starting a fresh 30 minutes.
The remaining seconds are stored with the new build-server ID in the existing
`queue_wait_limit_s` field, so a restart (including an older module reading the
store) retains that budget. The job cannot spill again on that attempt. A
caller-set budget is not extended, and gate jobs keep their fixed budget.
Refusals still include `spill_attempted=<code>` as described below. Tests:
`failed_spill_default_budget_waits_past_five_minutes_and_starts`,
`failed_spill_caller_budget_is_not_extended`,
`failed_spill_default_budget_is_capped_by_timeout`,
`failed_spill_budget_from_original_submission_survives_restart`.

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
| `runner_sign_missing`, `runner_sign_grant_refused` | The vault had no signing credential, or refused the grant. New jobs are refused for 30 seconds, then the next sign request asks the vault again; re-creating the credential or re-approving the grant does not need a restart. |
| `runner_sign_invalid_response`, `runner_sign_unavailable` | The vault sent a reply the agent could not use, or was unavailable. New jobs are refused for 2 seconds, then the next sign request asks the vault again. |

Tests: `missing_key_fails_health_and_run_before_snapshot_or_ssh`,
`refused_grant_fails_health_and_run_before_snapshot_or_ssh`,
`missing_enrollment_recovers_health_without_restart`,
`credential_backoff_doubles_caps_and_stops_on_shutdown`,
`agent_start_failure_recovers_without_restart`,
`transient_sign_failure_is_retried_after_backoff_and_health_recovers`,
`refused_sign_grant_is_retried_after_longer_backoff_once_reapproved`.

### Retry hints on credential refusals

Only a transient signing failure (`runner_sign_unavailable`,
`runner_sign_invalid_response`) puts the terminal record's optional
`retry_after_ms` on its refusal. The hint is the time left in the agent's
backoff for that failure: once it passes, the next job's sign request asks the
vault again, so a caller can wait and retry remotely instead of running the job
somewhere else. It does not promise the vault has recovered. Prepare replies
carry the same hint.

| Code | `retry_after_ms` |
| --- | --- |
| `runner_sign_unavailable`, `runner_sign_invalid_response` | The milliseconds left in the agent's 2-second backoff for that failure, rounded up: at least 1, at most 2000. |
| `runner_sign_missing`, `runner_sign_grant_refused` | Absent. An operator must re-create the credential or re-approve the grant; a wait-and-retry hint would make a caller retry in a loop. |
| `runner_credential_*`, `runner_agent_start_failed`, `runner_agent_stopped`, `runner_ssh_config_invalid` | Absent. Most need an operator to fix the vault, the agent or the SSH alias; the background retries above have no fixed time a caller could wait for. |

The hint is set wherever a connector refusal becomes a refusal: the check
before the snapshot, and a refusal found while opening the runner channel. Both
the agent route and `ck motor run` receive it, and `ck motor run` prints it in
its refusal line as `(retry after N ms)`. The field is omitted, not `null`,
when there is no hint, and older callers that know it read it as written. Tests:
`transient_sign_failures_hint_the_time_left_in_their_backoff`,
`repairable_sign_failures_carry_no_hint`,
`transient_sign_refusal_hints_the_backoff_left_and_the_hint_shrinks`,
`connector_retry_hint_reaches_run_and_prepare_refusals`,
`a_refusal_line_names_its_retry_hint_only_when_it_has_one`,
`oldest_production_client_decodes_new_refusal_tags_as_unknown_with_detail`.

### Retry hints on capacity refusals

A job the build server refuses because its queue wait ran out
(`refused_before_start`, reason `queue_wait_exceeded`) carries a
`retry_after_ms` hint when another job's completion is what frees it: enough
free permits, a running job's workspace, a queued job ahead in the same files,
or a job using this checkout as a sibling. The hint is the median queue wait of
the jobs the runner started in the last hour, clamped to 1000-60000 ms, and it
is absent when the job can never fit or when nothing started recently. A job
moved to hosted capacity by a spill withdrawal carries none: its caller is never
refused. The exact rules are in
[wire-protocol.md](wire-protocol.md#5-refusals-and-the-no-start-proof) (5.5 and
5.6). The field is the same one signing refusals use, so a client that already
reads it needs no change, and can wait out the hint and retry remotely instead of
running the job somewhere else. The runner computes the hint, so it appears once
the runner is deployed: `ck-motor` passes the hint through as the runner set
it.

A spilled job that no hosted runner received goes back to the build server
under a new build-server ID. If the build server then refuses it, at admission
or when its queue wait runs out, `ck-motor` appends
`; spill_attempted=<code>` to the refusal detail, where `<code>` is the broker
code that sent the job back (for example `hosted_capacity` or
`hosted_provisioning`), so the caller knows the hosted route was already
tried. If appending the note would exceed the refusal detail size limit, the
build server's part is shortened to leave room for it. A job that ran gets no
note, and a job that never spilled keeps the build server's refusal detail
unchanged. Tests:
`refusal_after_a_failed_spill_names_the_spill_attempt`,
`spill_attempt_note_keeps_within_the_refusal_detail_limit`.

Tests: `capacity_refusal_carries_the_recent_median_queue_wait_as_its_retry_hint`,
`capacity_retry_hint_is_clamped_to_its_bounds`,
`impossible_demand_refusal_carries_no_retry_hint`,
`capacity_refusal_without_recent_starts_carries_no_retry_hint`,
`spill_withdrawal_carries_no_retry_hint`.

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

## Caller submission keys

ck-motor advertises `exec-remote-submission-key/v1` and implements the caller
contract in `cortexkit-exec-remote-types` 0.2.14. A caller must check this
capability before sending `submission_key` or `resume_from_seq`: older executors
ignore those fields and could start the command twice.

An attested module caller may send an opaque `submission_key` of 16 to 128
printable ASCII bytes (`0x21` through `0x7e`, no whitespace). Invalid keys and a
`resume_from_seq` without a key are `invalid_params`, naming the offending field.
Keys are scoped to the daemon-attested module principal, never to a route,
session or connection. The principal/key, request digest and logical job ID live
in the job's durable journal record before any runner submission, including
hosted and Boat submissions. They expire together with the job, no earlier than
the job's 24-hour retention horizon. An unknown or expired key starts a fresh
submission; forgetting a key is not proof that an old request never ran.
A rollback to a ck-motor that predates keys rewrites its store without the key
records, so after rolling forward again, a key recorded before the rollback is
unknown and a resubmit with it starts a fresh job. The older ck-motor doesn't
advertise the capability, so callers send no keys while it runs.

Resubmitting the same request with a known key attaches to its existing logical
job and sends `Accepted` with `attached: true` and the existing job ID. A fresh
admission omits `attached`. An absent replay cursor starts at the beginning;
`resume_from_seq` is exclusive, unlike the inclusive `exec.attach.from_seq`.
Returned-byte expiration and outcome-unknown rules remain those of ordinary
attach. A spill that returns to the build server still has one key and one
caller-visible job ID, even though its runner-side ID changes.

The SHA-256 digest uses RFC 8785 canonical JSON of the caller request, before
snapshotting or converting return paths, with the key and replay cursor removed
and disallowed environment entries filtered out. It covers command, cwd,
workspace and repository paths, siblings, platform, weight hint, network,
timeout, queue-wait limit, forwarded environment and return options. JSON field
order and a newly captured checkout do not change the request. A changed request
is refused with `request_rejected` and detail `submission_key_mismatch`.

A recorded submission that provably never reached runner admission returns the
final refusal `submission_never_accepted`; the caller can choose a new key.
After a Run attempt, ck-motor uses the existing attach machinery to recover
runner records, not another Run. A missing recent build-server job is fenced by
the runner's durable attach tombstone. An unavailable runner is not evidence
that nothing ran. Every keyed refusal has its own job ID, never the earlier
job's; a mismatch does not alter the earlier job.

Only attested module principals can use keys today. subc's `Principal::Direct`
has no stable caller identity, and its bind identity is caller-supplied. A keyed
direct `exec.run` is refused before recording a key, as `request_rejected` with
detail `submission_key_requires_attested_caller`. `ck motor run` continues to
use its existing per-job secret files for reattach. Direct submission keys need
subc to attest a stable direct caller identity across reconnects; neither a
route handle nor a claimed session name can supply that boundary.

AFT heads and workers share `reserved:aft`. A head that knows a worker's key and
original request can therefore resume its detached job, as it can attach by a
known job ID. There is no new job enumeration or head/worker delegation API.

Source: `src/motor/submission_key.rs` and `src/storage.rs` under
`crates/motor-module`. Rules are covered by `known_key_replays_exclusively_and_does_not_recapture_checkout`,
`digest_is_canonical_and_covers_all_caller_execution_fields`,
`digest_mismatch_refuses_under_its_own_id_without_changing_job`,
`two_principals_have_separate_namespaces_and_stream_authority`,
`durability_before_runner_submission`,
`restart_between_key_record_and_submit_is_final_never_accepted`,
`restart_after_run_with_lost_ack_reattaches_runner_record`,
`run_fence_without_runner_admission_uses_attach_tombstone`,
`refused_key_survives_restart_and_different_request_still_mismatches`,
`runner_acceptance_is_remembered_before_network_echo_refusal`,
`concurrent_resubmits_start_one_job`,
`unknown_key_with_cursor_starts_a_fresh_submission`,
`keyed_spill_return_keeps_one_logical_job_across_restart`,
`keyed_hosted_route_reattaches_without_second_run`,
`submission_key_retention_matches_job_and_expired_key_is_unknown`,
`direct_submission_key_requires_an_attested_caller`,
`malformed_submission_fields_are_invalid_params_on_both_routes` and
`aft_head_route_can_resume_worker_submission_after_route_rebind`.

## Agent runs: hosted usage and returned files

These additions to AFT's `exec.run`, `exec.attach` and `exec.status` follow
`cortexkit-exec-remote-types` 0.2.14. Source: `crates/motor-module/src/agent_route.rs`.

**Hosted usage.** A job whose Run frame went to a hosted runner (first-party or
Boat) has `hosted` on its terminal record. It holds `placement`, the lease's
host (for example `ovh-1` or `boat:<sandbox_id>`), and `billed_vcpu_ms` once
billing has settled. Build-server jobs omit `hosted`, and so do spills that went
back to the build server. ck-motor adds no wait for billing. The job stream
already holds a first-party hosted terminal up to five seconds for the host's
settlement record, and settles a Boat job with the broker before sending its
terminal record. If billing is still pending after that, `billed_vcpu_ms` is
absent. A
later `exec.attach` re-sends the terminal record and `exec.status` returns it,
both with the figure as it stands at that moment.

**Returned files.** `return_paths` names files relative to the request's `cwd`;
`return_max_bytes` caps their total size, 262,144 bytes when absent. A larger
cap is refused as `invalid_params` before any job exists, whether or not paths
accompany it. Each path is joined to `cwd`, then the same lexical bounds as
direct runs apply before start: an absolute path, `..`, more than 64 paths or a
path outside the workspace is refused as `request_rejected` with
`return_paths_exceeded`. Hosted runners collect files in the same way as the
build server. The runner's result reaches the caller as a `return_result` record
immediately before the terminal record, with cwd-relative paths. Collection
failures (`over_cap`, `missing`, `escape`, `not_regular`, `frame_limit`) do not
change the job's outcome. A reattach from before the record's `seq` replays it.

Only a request with non-empty `return_paths` receives the record. This route
has no per-caller capability handshake: `started` goes to every caller, and
older callers skip it as an unknown record. ck-motor instead lists `exec-remote-return/v1` among
its capabilities, and a caller sends paths only to a module that lists it.
Callers built before 0.2.9 cannot send the field, so they never receive the
record.

**Chunked returned files.** A request that also sets `return_chunked` gets up
to 10 MiB of files (64 paths at most) instead of 256 KiB. `return_max_bytes`
then defaults to, and may be at most, 10,485,760; above 262,144 bytes it is
allowed only with `return_chunked`. Instead of `return_result`, the stream
carries `return_chunk` records (at most 512 KiB of one file each, all of one
file's chunks before the next file's, in request order) and then one
`return_manifest` immediately before the terminal record. The manifest lists
every requested path exactly as the caller sent it, with its size and SHA-256,
or a failure: `missing`, `not_regular`, `escape`, `over_cap` (with the file's
size; it would have passed the cap, which the runner decides before reading any
file, so it has no chunks) or `read_error` (the file changed size, or its path
named a different file, while it was read; its bytes are not sent). Paths are
collected through the same no-follow walk as `return_result`.

The runner keeps returned bytes for one hour after the job ends, in their own
directory beside the job record, written and synced before the record that
names them, like replayed output. Within that hour a reattach replays the same
chunks and manifest at the same seqs. After it, a replay sends no chunks and
the manifest at its original seq, with each returned entry turned into
`expired` (keeping its size) and `total_bytes` 0. That leaves a seq gap just
before the manifest; callers that set `return_chunked` accept it. The job
record itself still lives for the usual 24 hours.

ck-motor lists `exec-remote-return/v2` next to v1, and a caller sets
`return_chunked` only for a module that lists it. The runner advertises
support with `return_chunked` in its Hello. A runner without it (an older
build-server, tenant or Boat image, or a Windows runner, which refuses returned
files altogether) gets no Run: the job is refused before start as
`request_rejected` with `return_chunked_unsupported`. Callers that do not set
the flag get exactly the `return_result` behaviour above. `ck motor run` does
not offer chunked return.

Tests: `agent_route::tests` in `crates/motor-module/src/agent_route_tests.rs`
(hosted usage, settled and pending billing, a hosted return result, the chunked
cap and path echo), `crates/motor-module/tests/it/return_paths.rs`
(`return_chunked_agent_caller_gets_a_verified_430_kb_file`,
`return_paths_agent_v1_caller_gets_only_return_result`,
`return_chunked_runner_without_capability_refuses_before_run`),
`crates/motor-runner/src/tests/return_paths.rs`
(`return_chunked_430_kb_file_round_trips_through_replay_and_restart`,
`return_chunked_replay_after_one_hour_skips_chunks_and_leaves_a_seq_gap`),
`crates/motor-runner/src/return_paths.rs` (the 10 MiB cap, growth and a file
swapped for a symlink), and in
`crates/motor-module/tests/it/operator_cli.rs`
`agent_return_paths_send_return_result_immediately_before_the_terminal`,
`agent_without_return_paths_never_receives_a_return_result`,
`agent_over_cap_return_max_bytes_is_invalid_params_and_never_runs`.

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
as 64 lowercase hex characters. Keep the secret private: possession grants
attach, status and cancel for that job, not submission or access to any other
job. Without `--secret-file`, `run` saves the secret in a new file named by job
ID under a per-user `0700` directory in ck-motor's data root, and prints only
the job ID and file path to stderr. `attach JOB_ID` discovers that default file
automatically. For scripts that need a caller-selected path, use:

```sh
ck motor run --secret-file /private/job.key -- 'cargo test'
ck motor attach JOB_ID --secret-file /private/job.key
ck motor attach JOB_ID --secret-file /private/job.key --status
ck motor cancel JOB_ID --secret-file /private/job.key
```

`--secret-file` on `run` creates a new file with mode `0600`, refuses an existing
file (including a symlink), and prints its path instead of the secret. Default
files are created exclusively with mode `0600`, removed when a terminal record is
received, and swept once they exceed the 24-hour job retention. If no default
file exists, `attach` reads one line from stdin; `--secret-file PATH` selects an
explicit file. Never place the secret in argv or an environment variable; neither
is an accepted input channel. Do not enable shell tracing while handling it.

`cancel JOB_ID` stops a job whose `ck motor run` process is gone, for example
after it was killed, without resubmitting anything. It finds the secret exactly
as `attach` does: the default file named by the job ID, an explicit
`--secret-file`, or one line on stdin. It sends the direct `exec.cancel`, which
ck-motor admits only for the route that started the job or a caller holding the
job's secret; there is no other fallback, and the secret is never printed. The
result is one line on stdout:

- `cancelled` — the cancel is durable and the job had not ended yet.
- `already finished: OUTCOME (job_id=ID)` — the job had ended; `OUTCOME` is its
  exit code, signal, timeout, cancellation or refusal.
- Nothing on stdout when the cancel is refused, with the reason on stderr,
  under the same wording an unknown job produces.

The exit status is 0 when the job is cancelled or had already ended, 200 when
the cancel is refused (an unknown job ID and a wrong secret get the same
refusal), 201
when the cancel may not have reached ck-motor, and 204 for bad arguments. The
command does not wait for the job to end; `attach JOB_ID` then shows its final
output. A saved default secret file is kept, so `attach` can still read it.

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
`ck motor cancel` is covered by `cli_cancel_ends_a_running_job_with_killed_cancel`,
`cli_cancel_reports_a_finished_job_and_exits_zero`,
`cli_cancel_of_an_unknown_job_fails_without_runner_io` and
`cli_cancel_with_a_wrong_secret_is_refused_like_an_unknown_job` in the same
file. CLI unit tests: `secret_file_is_private_new_and_never_overwrites`,
`attach_never_accepts_an_argv_secret_or_echoes_it`,
`cancel_never_accepts_an_argv_secret_or_echoes_it`,
`cancel_line_names_each_way_a_job_can_have_ended`.

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
direct runner after a hosted failure; an unhinted `no_host_fits` answer, described below,
is a routing correction, not a failure. Windows stays on its direct runner. Outbound-network jobs can use
hosted leases: the request's `net` flag must match the signed lease claim, and
`network_routing` must still be enabled. Unlisted jobs retain their existing
direct and spill behavior; spilled leases and shadow leases send `allow_spill:
true`.

Set `hosted_repositories_placement_hint` to send exactly that `{host, vm}` hint
on repository-routed leases, overriding any cached warm placement. The setting
is omitted from serialized Config when unset, so older clients with closed
schemas can still read the default config. Spill, shadow and fleet-routing leases
never inherit the setting, and placements from hinted jobs are not cached as warm
hints for other jobs. The hint is saved with the job's route before leasing, so
retries and restarts retain it even if the config changes. With a broker host
marked `explicit_only`, only a hint naming that host admits the job there; if it
does not fit, the broker returns retryable `no_capacity` rather than choosing
another host. Such a refusal waits and retries within the normal queue budget.
A hinted repository job never corrects to the build server, even on
`no_host_fits`, and still sends `allow_spill: false` so it cannot run on Boat.

For a repository-routed job, broker `503 no_capacity` responses wait for
`Retry-After` (seconds, clamped to 1–30 seconds; missing or invalid means one
second) before retrying the identical request, including its job ID. Capacity
and `202 provisioning` retries share one queue-wait deadline. Exhaustion yields
a typed retryable refusal, never local execution. The broker can grant a lease
from a stale view of the host, which then refuses the job as full (`host_full`,
or `capacity`). A routed job waits two seconds and asks again with the same job
ID and lease body, within the same queue-wait deadline. The contract allows
this: a re-request before admission returns the same reservation and placement
with a fresh run token. These waits don't count against the three placement
attempts. When the deadline passes, the job gets the same refusal as for
`no_capacity` (`hosted_capacity`, runner full, retry after 2 s), and the unused
reservation is released. That holds when the deadline cuts off the lease request
that asks again after a `host_full` refusal: the job was waiting for the host, so
it is refused as runner full, not unreachable. A broker that has not answered
the job's first lease request when the deadline passes gives
`hosted_provisioning` (unreachable). Before the durable `run_sent`
marker, refusals identify repository-selected routing with the exact detail text
`hosted stage-1 routing (hosted_repositories)`.
After that marker the existing hosted reattach/finish path governs an ambiguous
submission.

A broker answer of `no_host_fits` (HTTP 422, `{"error":"no_host_fits",
"max_vcpus":N}`) means no host it could ever use holds the job's vCPUs, so a
retry cannot succeed. For an unhinted repository-routed job that has not sent its Run
frame, this undoes the routing choice rather than failing the job: ck-motor
removes the job's hosted route, stores `routed_direct_reason: "no_host_fits"`
on the job, logs one INFO event `hosted no_host_fits; routing to build server`
with `job_id`, `vcpus` and `max_vcpus` (`unknown` when the broker omitted it),
and submits the job to the build server like any unlisted job. The lease is not
retried, and nothing is released or finished because nothing was booked. The
stored decision survives restart, so a later attach reads the job from the build
server. Time already spent waiting on the broker is taken off the job's
queue-wait limit once, in whole seconds, and the reduced limit is both what the
Run frame carries and what ck-motor enforces; a positive limit never drops below
one second. The job does not spill. For spilled and shadow leases
`no_host_fits` is an ordinary refusal: a spill goes back to the build server
and a shadow is skipped, each after one lease request. The error code decides;
a missing or non-numeric `max_vcpus` is read as unknown. Each selected job logs one INFO event, `routed to hosted
(hosted_repositories)`, with `job_id` and `repo` fields. Hosted health metrics
include `hosted_routed_jobs_1h`, counting repository-selected jobs (including
refusals) in the last hour, and `hosted_no_host_fits_1h`, counting
repository-selected jobs sent to the build server after `no_host_fits` in the
last hour (these are not in `hosted_routed_jobs_1h`); stored route decisions
survive restart.

A lease refused with HTTP 403 `{"error":"account_closed"}` means the hosted
account was closed for good: none of its jobs may run anywhere. On every route
(configured, repository-routed, spilled, shadow or a direct hosted test) the
lease is not retried and the job is refused before start with
`RequestRejected` and detail `hosted_account_closed: hosted account closed`,
with no retry delay and no local or direct fallback. A spilled job is not sent
back to the build server either, unlike after `no_capacity`. Only lease
creation carries this code; a 403 elsewhere stays `hosted_auth`. A job the
broker placed before the closure keeps its normal answers. Tests:
`cloud_account_closed_lease_is_one_final_request_on_every_route`,
`closed_account_rerequest_of_a_placed_job_still_runs_but_a_new_job_is_refused`,
`spilled_account_closed_is_refused_and_never_resubmitted`.

Tests: `listed_main_and_linked_agent_and_direct_jobs_use_hosted_without_avoiding_a_host`,
`unlisted_and_windows_jobs_keep_the_direct_path`,
`stage1_pre_run_failures_are_typed_and_never_submit_directly`,
`stage1_lost_run_ack_reattaches_without_direct_fallback`,
`stage1_no_capacity_retries_the_identical_body_then_runs_on_the_host`,
`stage1_no_capacity_and_provisioning_share_one_bounded_queue_deadline`,
`stage1_host_full_waits_and_asks_again_until_the_host_admits_the_job`,
`stage1_host_full_until_the_queue_budget_ends_refuses_like_no_capacity`,
`stage1_queue_budget_ending_during_a_lease_request_refuses_for_what_the_job_waited_on`,
`stage1_no_host_fits_runs_on_the_build_server_once_and_fitting_jobs_stay_hosted`,
`stage1_no_host_fits_decision_survives_restart_and_attaches_to_the_build_server`,
`stage1_no_host_fits_after_a_capacity_wait_sends_the_remaining_queue_wait`,
`spill_no_host_fits_resubmits_to_the_build_server_without_retrying`,
`shadow_no_host_fits_is_one_request_with_or_without_max_vcpus`,
`no_host_fits_decision_trims_queue_wait_once_and_is_counted_for_an_hour`,
`unlisted_spill_eligible_job_still_withdraws_and_sends_allow_spill_true`,
`shadow_lease_explicitly_allows_spill`, and
`hosted_routed_counter_only_counts_repository_routes_in_the_last_hour`.
Placement-hint coverage:
`repository_placement_hint_config_roundtrips_absent_null_and_present`,
`repository_placement_hint_rejects_invalid_names_and_null_vm`,
`repository_routed_lease_carries_the_configured_hint_without_caching_it_for_other_routes`,
`hinted_no_capacity_retries_the_identical_body_then_runs_without_fallback`,
`hinted_no_capacity_exhausts_the_queue_budget_without_fallback`,
`hinted_no_host_fits_refuses_without_build_server_correction`,
`unlisted_spill_eligible_job_still_withdraws_and_sends_allow_spill_true`,
`shadow_hanging_or_refused_never_changes_callers_stream_or_exit`, and
`fleet_linux_default_platform_jobs_allow_hosted_spill_but_gates_stay_direct`.
The canonical `no-spill-lease.json` fixture from `cortexkit-cloud` is pinned under
`crates/motor-lease/contracts/cloud/`. Tests
`cloud_no_spill_request_serializes_to_the_exact_canonical_bytes_and_hash`,
`stage1_cloud_no_spill_reply_honors_retry_after_and_preserves_the_stored_request`,
and `stored_allow_spill_presence_and_value_survive_lease_re_requests` check its
hash projection, capacity reply, and stable re-request policy. Requester keys are
excluded from the lease hash; an absent `allow_spill` and either explicit boolean
have distinct hashes, so the stored request preserves presence as well as value.
`non_null_hint_keeps_the_pinned_broker_canonical_member_order` checks a non-null
hint derived from the no-spill vector. The `cortexkit-cloud` fixture
`explicit-only-leases.json` at commit `54042e7` pins two route-generated leases
with the non-null hint `{"host":"explicit","vm":"warm"}`. Tests
`cloud_explicit_only_leases_match_their_pinned_hashes` and
`cloud_explicit_only_lease_bodies_match_the_pinned_hashes` check motor-lease's hash
projection from the pinned request bytes and ck-motor's generated lease bodies
against both broker-supplied hashes. The separate tests
`derived_non_null_placement_hint_matches_the_cross_check_hash` and
`derived_non_null_placement_hint_lease_body_matches_the_cross_check_hash` cover
the derived `staging-fra-1`/`t1` body; it is not a route-generated vector.
CLOUD's `no-host-fits-lease.json` (from `cortexkit-cloud` `916347e`) is pinned
beside it: `stage1_cloud_no_host_fits_reply_routes_the_job_to_the_build_server`
sends its exact 422 reply bytes through ck-motor's broker client, and
`cloud_no_host_fits_reply_bytes_match_fake` checks the fake broker gives the
same answer.
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
