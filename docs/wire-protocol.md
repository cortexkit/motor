# Contract: the wire protocol

`ck-motor` and `motor-runner` talk over one framed protocol, version 1. The
client runs `motor-runner connect` over SSH, which bridges to the runner's Unix
socket; operators use the same socket for `drain` and `resume`. This contract
defines the framing, the frames, the closed vocabularies, and the rules for
deciding whether a command ran.

Two layers share these words:

- The caller-facing types (`RunRequest`, `StreamRecord`, `TerminalRecord`,
  `Outcome`, `RefusalReason`, `Ran`, `Killed`, `Platform`, `Network`) come from
  the published crate `cortexkit-exec-remote-types`, re-exported by
  `motor-protocol`. Callers such as AFT see these through the `exec-remote/v1`
  module operations.
- The frames below are motor's own, in `crates/motor-protocol/src/types.rs` and
  `codec.rs`. They carry snapshots, queue control and job streams between
  `ck-motor` and the runner.

The synthetic corpus in [`test-vectors/exec-remote-v1/`](../test-vectors/exec-remote-v1/README.md)
pins example bytes for every frame and outcome. Rules below name the test that
proves them; protocol tests live in `crates/motor-protocol/tests/it/`.

## 1. Framing

1.1. A frame is a four-byte big-endian unsigned length, then exactly that many
bytes of UTF-8 JSON. No delimiter or newline follows. Test:
`codec_pins_big_endian_prefix_and_detects_truncation` (`contract.rs`).

1.2. The JSON body may be at most 1 MiB (`MAX_FRAME_BYTES`, 1048576 bytes). A
body of exactly that size is accepted. A larger length prefix is refused before
the body is read or allocated, and a writer refuses to emit an oversized frame
without writing anything. Tests:
`frame_limit_accepts_boundary_and_bundle_chunks_and_rejects_oversized_writes`,
`oversized_header_is_refused_before_reading_body`,
`asynchronous_oversized_header_is_refused_before_reading_body`.

1.3. End of stream at a frame boundary is a clean close; inside a frame it is an
error. Test: `codec_pins_big_endian_prefix_and_detects_truncation`.

1.4. Byte payloads (`bytes`, `data`) are standard padded base64 and may hold
any bytes, including split or invalid UTF-8. Other base64 forms are refused.
Test: `byte_payloads_preserve_split_utf8_and_reject_invalid_base64`.

1.5. `job_id` and `transfer_id` are UUIDv7 strings. The codec checks UUID
syntax; the version is the producer's policy. Not covered by a test.

## 2. Frames

Every frame is a JSON object with a `type` field; its other fields sit beside
it.

| `type` | Direction | Purpose |
| --- | --- | --- |
| `hello` | both | Protocol version and capabilities (section 3). Always the first frame. |
| `have` / `have_reply` | client → runner / reply | Which commits and Git dependencies the runner already has. |
| `run` | client → runner | Submit a job: workspace, command, filtered environment, snapshot commit, tree hash, siblings, bundle manifest, optional `weight_hint`, `timeout`, `queue_wait_limit_s`, `platform`, `network`, `workspace_kind`, `gate_job`. |
| `accepted` | runner → client | The job is durably accepted; carries queue position, granted network and platform echo. |
| `bundle_chunk` | client → runner | Bundle bytes by bundle index and offset. |
| `started` | runner → client | The job took capacity and began; carries `seq`, queue wait, start time, optional `path_mapping`. |
| `output` | runner → client | Sequenced stdout or stderr bytes. |
| `heartbeat` | runner → client | Keeps the stream alive; optional `queue_wait_ms` while waiting for capacity. |
| `terminal` | runner → client | The job's final record. Ends the job's stream. |
| `refusal` | runner → client | Refuses a `run` (with `job_id`) or `prepare` (with `transfer_id`) before start: `reason`, optional `detail`, `retry_after_ms`, `disk_full`. |
| `withdraw` / `withdraw_reply` | client → runner / reply | Ask to withdraw a queued job; answer `withdrawn` or `started`. |
| `attach` | client → runner | Replay and follow a job's stream from `from_seq`. |
| `cancel` / `cancel_reply` | client → runner / reply | Durable, idempotent cancellation. |
| `prepare`, `prepare_accepted`, `prepared` | both | Prepare a workspace without a command. |
| `drop` / `drop_reply` | both | Remove a workspace; reply has `dropped` and `cancelled_jobs`. |
| `status` / `status_reply` | both | Aggregate runner status. |
| `job_status` / `job_status_reply` | both | One job's state. |
| `drain` / `drain_reply`, `resume` / `resume_reply` | operator ↔ runner | Close and reopen admission; see [drain-and-rollback.md](drain-and-rollback.md). |

## 3. Hello and capabilities

3.1. Each side sends `hello` with `protocol_version: 1`. On a mismatch the
runner replies and closes; the client refuses the job with
`runner_version_mismatch` and does not retry. Test:
`version_mismatch_does_not_retry_the_handshake`
(`crates/motor-module/src/transport_retry_tests.rs`).

3.2. Capabilities are optional fields of `hello`. Absent means false or empty,
and a false or empty value is omitted, so a peer that knows none of them sends
the original bytes. Test: `hello_without_started_capability_keeps_legacy_wire_bytes`.

| Field | Meaning when set |
| --- | --- |
| `started: true` | The client accepts `started` records (section 6). |
| `default_branch_tip: true` | The client's integration tip is its local default branch, so the runner may use it for cache fills. Test: `hello_default_branch_tip_capability_is_additive`. |
| `gate_jobs: true` | The peer understands `run.gate_job`; see [scheduling.md](scheduling.md#queue-order). Test: `gate_capability_and_mark_are_additive_motor_only_fields`. |
| `platforms: ["windows"]` | The runner serves Windows jobs; see [windows-jobs.md](windows-jobs.md). Test: `windows_hello_advertises_explicit_platforms`. |

3.3. Unknown object fields in any frame are ignored. Test:
`all_frame_and_reply_types_ignore_unknown_fields`.

3.4. An unknown frame `type` is skipped by the reader, and its `seq`, when it
is a valid unsigned integer, still advances the resume cursor. A known `type`
with malformed fields is an error, never treated as unknown. Tests:
`protocol_reader_skips_future_types_and_reads_resume_frames`,
`tolerant_decoder_uses_supported_tags_and_preserves_future_cursors`,
`malformed_known_tags_and_invalid_unknown_sequences_still_fail`.

## 4. Outcomes

A job's `terminal` record has an `outcome` object discriminated by `type`. The
vocabulary is closed in this version; a new kind decodes as unknown.

| `outcome.type` | Fields | Meaning |
| --- | --- | --- |
| `exit` | `code` (i32) | The command exited on Linux. |
| `windows_exit` | `code` (u32) | The command exited on Windows; see [windows-jobs.md](windows-jobs.md#6-exit-status). |
| `signal` | `signal` | A signal killed the command. |
| `cancelled` | | Cancelled before it started. |
| `refused_before_start` | `reason` | Refused; the command did not start (section 5). |
| `outcome_unknown` | | The command may have run; its result is lost. |
| `history_expired` | | The job's record is past retention. Says nothing about whether it ran. |
| any other | | Unknown: grade it as `outcome_unknown`. Test: `unknown_outcome_does_not_claim_a_before_start_proof`. |

The record also carries:

- `ran`: `"remote"` or `"none"`; `null` only for `history_expired`. An unknown
  value must never be read as `"none"`. Test: `unknown_ran_is_not_a_guarantee_of_no_execution`.
- `killed`: `"deadline"` or `"cancel"` when the runner killed a running command;
  omitted otherwise. Unknown values are kept. Test: `unknown_killed_reason_is_retained`.
- `tree_hash` (null before hashing or after expiry) and `workspace_changes`
  (an array only when `ran` is `"remote"`; an empty array means no changes).
  Both are always present, explicitly null when unavailable. Test:
  `outcome_vectors_pin_before_start_proofs_and_nullability`.
- Optional `pipestatus`, `refusal_detail` (at most 1024 bytes, for display,
  never for parsing), and `retry_after_ms`.

## 5. Refusals and the no-start proof

5.1. Only `refused_before_start` proves that the command did not start. Every
other outcome, including `outcome_unknown`, `history_expired` and an unknown
kind, must be treated as "may have run": never re-run the command
automatically. Tests: `outcome_vectors_pin_before_start_proofs_and_nullability`,
`expired_history_never_becomes_before_start_refusal`
(`crates/motor-module/tests/it/recovery.rs`).

5.2. Any reason inside `refused_before_start`, including one this version does
not know, still proves no start. Test: `unknown_refusal_retains_the_before_start_arm`.

5.3. The typed reasons, with their wire strings:

| Reason | Produced when |
| --- | --- |
| `unreachable` | `ck-motor` could not reach the runner or complete `hello`. |
| `runner_version_mismatch` | `hello` versions differ. |
| `snapshot_failed` | Capture failed or the untracked-size cap was exceeded; see [snapshot-contents.md](snapshot-contents.md). |
| `workspace_key_rejected` | The workspace, repository or `cwd` paths are invalid. |
| `runner_full` | The queue is full, the vCPU claim cannot fit the host, or the Windows VM slot is busy. |
| `runner_draining` | The runner is draining. |
| `runner_disk_full` | Free disk space is below the floor. |
| `queue_wait_exceeded` | The job waited longer than its queue-wait budget. |
| `transfer_interrupted` | The upload was cut before the runner had every bundle, or an upload or fetch stalled. |
| `tree_hash_mismatch` | The runner's checkout does not match the snapshot's tree hash. |
| `bundle_rejected` | A bundle failed verification or fetch. |
| `workspace_setup_failed` | Workspace setup or isolated-job launch failed, or persistence/output setup failed before the execution gate opened. |
| `network_unsupported` | Outbound network was requested and is not available. |
| `platform_unsupported` | The requested platform is not served. |

5.4. The reason is a stable tag; human diagnostics belong in `refusal_detail`,
not in the reason string. A queue-wait refusal uses `queue_wait_exceeded` and
reports `waiting_on`, permit holders and other scheduler context in the detail.
A setup refusal uses `workspace_setup_failed` and puts the setup step and error
in the detail. Details are bounded and redacted as in rule 4; do not parse them
for policy. Tests: `queue_wait_refusal_names_permit_holders_and_their_start_time`,
`workspace_setup_refusal_preserves_step_and_error_in_fallback_terminal`,
`setup_refusal_detail_is_bounded_redacted_and_not_part_of_the_reason`,
`oldest_production_client_keeps_typed_queue_and_setup_refusal_details`.

Several diagnostic codes share an existing typed reason:

| Code retained in detail | Typed wire reason | Produced when |
| --- | --- | --- |
| `transfer_stalled` | `transfer_interrupted` | An upload or fetch made no progress for 60 seconds, or a client withdrew an incomplete transfer with the private stalled-transfer marker. |
| `windows_slot_busy` | `runner_full` | The host-wide Windows VM slot is held by another VM. |
| `start_not_persisted` | `workspace_setup_failed` | Start persistence failed while the execution gate was still closed. |
| `output_unavailable` | `workspace_setup_failed` | Output handles could not be acquired while the execution gate was still closed. |

These mappings do not add retry hints. A private `withdraw` request with the
`transfer_stalled` marker can withdraw a pending job only while it is still
AwaitingBundles; its resulting terminal uses `transfer_interrupted`. A queue
withdrawal instead requires a queued job whose queue-wait limit has expired.
A release failure that may have opened the gate remains `outcome_unknown`, never a
before-start refusal. Tests: `upload_stall_bound_ignores_heartbeats_and_empty_chunks`,
`fetch_timeout_refusal_is_typed_and_keeps_the_stall_code`,
`stalled_withdrawal_keeps_private_intent_and_typed_terminal`,
`windows_backend_slot_busy_comes_from_child_code_75_before_hello_deadline`,
`started_write_failure_stops_and_reaps_unreleased_execution`,
`output_handle_failure_stops_and_reaps_unreleased_execution`,
`partial_release_failure_stops_and_reaps_without_no_start_claim`.

Motor still emits the following reason strings as unknown reasons (rule 5.2
still holds):

| String | Produced when |
| --- | --- |
| `lost_to_restart` | A runner restart found a foreground job queued and never started; internal interrupted background fills also use this tag. |
| `prepare_unsupported` | `prepare` was sent to a Windows instance. |
| `direct_timeout_refused` | A direct `ck motor run` asked for no timeout or one above the configured cap. |
| `runner_credential_*`, `runner_agent_*`, `runner_sign_*`, `runner_ssh_config_invalid` | `ck-motor` cannot authenticate to the runner; see [module.md](module.md#credential-failures). |

Older runners can still send reason strings such as `queue_wait_exceeded: ...`
or `workspace_setup_failed: step: error`, or use the specific cause codes
`transfer_stalled`, `windows_slot_busy`, `start_not_persisted` and
`output_unavailable` as reasons. Decoders preserve these as unknown reasons
rather than rejecting them. Do not match their human-text prefixes for execution
or retry decisions. Test:
`unknown_refusal_retains_the_before_start_arm`.

5.5. `retry_after_ms` is a hint that a refusal is transient (draining sends the
time left to the drain deadline). Disk-full refusals carry no hint, because
nothing re-probes on a schedule. A started job's terminal never carries one.
Tests: `drain_retry_hint_is_bounded_by_the_deadline_on_the_private_frame`,
`retry_hint_is_removed_from_started_job_terminal`.

## 6. Sequence numbers, started and output

6.1. Records on a job's stream (`started`, `output`, and unknown sequenced
records) share one increasing `seq`. When the client negotiated `started`, the
`started` record is `seq` 0 and output begins at 1. Test:
`started_vectors_share_output_sequence_and_replay_boundaries`.

6.2. `started` is sent once, after setup, when the job takes capacity, before
any output. A job refused before start never has one. The command's `timeout`
runs from this moment. Tests: `started_is_recorded_only_after_setup_with_capacity_wait`,
`live_started_frame_is_sent_once_before_output`,
`refused_before_start_has_no_started_record` (`crates/motor-runner/src/tests/started.rs`).

6.3. A client that did not negotiate `started` receives no `started` records,
and every sequence number is shifted down by one, so its output still counts
contiguously from 0. Tests: `legacy_replay_projects_cursors_and_real_truncation_but_not_terminal`,
`legacy_replay_is_identity_for_refused_and_pre_started_records`.

6.4. `ck-motor` never infers a start from heartbeats when the runner did not
send `started`. Test: `new_ck_motor_old_runner_does_not_guess_started_from_heartbeats`.

6.5. `output` carries `stream` (`stdout` or `stderr`; unknown names keep their
bytes and `seq`) and raw bytes. Test: `unknown_output_stream_preserves_raw_bytes_and_sequence`.

## 7. Attach, replay and reattach

7.1. `attach {job_id, from_seq}` replays retained records with `seq >= from_seq`,
then follows the live stream until `terminal`. Replay is inclusive and
contiguous; `terminal` comes only after every earlier record. Tests:
`replay_pages_keep_inclusive_contiguous_cursors_and_defer_terminal`
(`crates/motor-runner/src/tests/monitoring.rs`),
`started_replay_is_inclusive_durable_and_not_repeated`.

7.2. When `ck-motor` loses its connection, it reattaches from one past the last
`seq` it delivered, so the caller sees each record once. An unknown record's
`seq` counts; a malformed reply does not move the cursor. Tests:
`replay_cursor_resumes_at_eight_and_independent_attach_is_inclusive`,
`unknown_record_seq_advances_reattach_cursor_before_next_known_output`,
`malformed_resume_reply_does_not_advance_reattach_cursor`,
`started_advances_reconnect_cursor_and_duplicates_are_ignored`.

7.3. When retained output has been evicted, the first replayed `output` carries
`truncated_before_seq`, the sequence number before which records were dropped,
instead of replay silently skipping them. Test:
`legacy_replay_projects_cursors_and_real_truncation_but_not_terminal`.

7.4. Once `ck-motor` has attempted to send `run`, it never resubmits it and
never invents a before-start outcome. If the acknowledgement is lost, it
attaches to the same job ID to learn what happened. Tests:
`lost_upload_a_before_runner_reads_run`, `lost_upload_b_after_accepted`,
`lost_upload_c_midway_second_bundle`, `lost_upload_d_first_verified_second_held`
(each ends as `transfer_interrupted` with `ran: none` and runs nothing; a
replayed `run` returns the same terminal), `lost_upload_e_after_last_byte_runs_once`.

7.5. A caller that disconnects detaches; the job keeps running and its history
stays retrievable for 24 hours. Test: `cancelled_request_detaches_and_the_job_keeps_running`
(`crates/motor-module/tests/it/detach.rs`).

## 8. Queue wait and withdrawal

8.1. `heartbeat.queue_wait_ms` is present only while the job waits for
capacity, measured from when its bundles were verified and pinned. It is absent
during upload, fetch, setup and execution. A plain heartbeat never authorizes
withdrawal. Tests: `queue_heartbeats_are_additive_in_both_upgrade_orders`,
`only_observed_permit_wait_can_withdraw_not_elapsed_acceptance`
(`crates/motor-module/src/queue_tests.rs`).

8.2. `withdraw {job_id}` with no reason asks to withdraw a job past its
queue-wait limit; `reason: "transfer_stalled"` withdraws a job whose upload
stalled, only before dispatch. The `withdraw_reply` is authoritative:
`withdrawn` means the job ended `refused_before_start`; `started` means it was
already running or not eligible, and the client follows it. Test:
`zero_queue_wait_withdraws_and_cancel_is_idempotent`.

## 9. Cancel

9.1. `cancel {job_id}` is durable and idempotent; the reply acknowledges the
request, and the terminal record arrives on the job's stream. Cancelling an
unknown ID records a tombstone. Cancellation requested before the runner has
seen the job survives a `ck-motor` restart. Tests:
`zero_queue_wait_withdraws_and_cancel_is_idempotent`,
`cancel_before_seen_survives_module_restart_and_attach_cancel`,
`cancel_while_second_bundle_is_held_preserves_cancelled_tombstone`.

## 10. Compatibility

10.1. Every addition in this version is optional and omitted at its default,
so old and new peers interoperate in both directions. Tests:
`optional_queue_and_retry_fields_work_in_both_upgrade_orders`,
`linux_run_without_platform_keeps_legacy_wire_bytes`,
`hello_empty_platforms_keep_both_legacy_wire_shapes`, and
`scripts/mixed-version.py`, which builds the client pinned in
`scripts/compat-client-revision` and runs it against the current runner and the
reverse.

10.2. The protocol corpus in `test-vectors/exec-remote-v1/` is checked byte for byte by `golden_vectors_are_complete_and_canonical`
and `sha256sums_covers_every_corpus_file`, and regenerated by
`python3 crates/motor-protocol/tests/generate_vectors.py`.
