# Scheduling, capacity and disk admission

This document describes how the runner decides which job runs next, which CPUs
it gets, when a job is refused for capacity or disk, and how long it keeps job
history and workspaces. The code is in `crates/motor-runner/src/engine.rs`,
`cpu.rs` and `disk.rs`. [architecture.md](architecture.md) shows where these
parts sit; [wire-protocol.md](wire-protocol.md) defines the refusals they
produce.

## vCPU claims

- A job claims exactly its `weight_hint` vCPUs, or 2 when the request has none.
  The command's own flags and environment (such as `-j` or `CARGO_BUILD_JOBS`)
  do not change the claim. Test: `claims_are_exact_and_unsized_jobs_always_claim_two`
  (`crates/motor-runner/src/tests/vcpu.rs`).
- A claim of 0, or larger than the host's allocatable CPUs, is refused before
  start with `runner_full` and a detail naming the limit. Tests:
  `oversized_and_zero_claims_are_refused_before_start_with_limit_detail`,
  `oversized_vcpu_refusal_detail_reaches_the_run_terminal`
  (`crates/motor-module/tests/it/recovery.rs`).
- A claim that fits the host but exceeds `max_weight` is accepted but waits
  until it fits; it does not block other jobs. Test:
  `pinned_job_waits_until_its_exact_claim_fits_max_weight`.
- The background warm build claims up to 4 vCPUs, fewer on a smaller host.
  Test: `internal_warm_build_shrinks_to_a_small_host`.

## The CPU allocator

The runner gives each Linux job specific logical CPUs, records them in the job
record before setup, and pins the job's service to them with `AllowedCPUs=`.

- The topology comes from the host's online CPUs and their hyperthread
  siblings. On a host with at least four physical cores, the first core and its
  siblings are kept for the runner and the system; smaller hosts keep nothing
  back. Tests: `topology_reads_online_siblings_and_reserves_cpu_zero_core`,
  `small_hosts_keep_every_cpu_allocatable` (`crates/motor-runner/src/cpu_tests.rs`).
- Placement prefers whole free sibling pairs; an odd claim prefers a pair that
  is already split. Tests: `even_claims_use_whole_pairs_without_overlap_while_free`,
  `odd_claims_prefer_an_already_split_pair`,
  `fragmented_even_claim_uses_free_threads_before_overlap`.
- When no free CPUs are left, a claim overlaps the least-claimed CPUs. CPU sets
  are therefore exclusive only while the host has free CPUs. Test:
  `overlap_uses_least_claimed_cpus_only_after_free_cpus`.
- Recorded CPU sets of jobs still running are registered again after a runner
  restart. Test: `recorded_live_cpu_sets_reregister_after_restart`.
- The service's memory cap follows the claim; see
  [job-isolation.md](job-isolation.md#7-limits). Test (operator-only):
  `isolated_four_vcpus_enforce_cpu_set_and_memory_max`.

## Capacity

- `max_weight` (default 28; `serve --max-weight N`) bounds the sum of vCPU
  claims of running jobs and setups in progress. Each setup counts as soon as it
  is chosen. Test: `dispatch_indexes_add_each_starting_weight_and_cpu_claim_before_selecting_next`
  (`crates/motor-runner/src/tests/gate_priority.rs`).
- `max_queue_depth` (default 64; `serve --max-queue-depth N`) bounds jobs that
  are accepted but not yet running, including those still uploading. A request
  beyond it is refused with `runner_full` before any `accepted` record. Tests:
  `health_counts_admission_refusals_without_acceptance`,
  `queue_depth_counts_sixty_four_awaiting_bundles`
  (`crates/motor-module/tests/it/recovery.rs`).
- The Windows instance fixes every job's claim and `max_weight` at 4, so it runs
  one job at a time; see [windows-jobs.md](windows-jobs.md). Test:
  `windows_weights_are_fixed_and_warm_builds_and_caller_catalog_are_disabled`.

## Queue order

A workspace runs one job at a time. Its oldest waiting job is its head; the
dispatcher chooses among heads:

1. Gate jobs (direct `ck motor run` jobs, marked only when both sides negotiated
   the `gate_jobs` capability), in arrival order. Test:
   `gate_gets_next_eight_permits_before_older_workers_and_later_gates`,
   `gate_mark_is_honored_only_after_capability_negotiation`.
2. Heads that have waited 60 seconds or more, oldest first.
3. Newer heads, rotating between workspaces by least recent dispatch.
4. Background warm builds and cache fills, last.

When a head does not fit the free capacity, smaller heads behind it may run in
its place (backfill) for its first 60 seconds. After that, or when the blocked
head is a gate job, backfill stops until it fits. A head blocked by its own
workspace (a checkout in use, a deletion, a sibling conflict) does not stop
others. Running jobs are never stopped or moved to other CPUs, and no capacity
is permanently reserved. Tests: `capped_head_allows_backfill_only_until_its_grace_expires`
(`crates/motor-runner/src/tests.rs`), `capacity_blocked_gate_still_fences_smaller_workers`,
`workspace_conflicted_gate_does_not_block_independent_workers`,
`waiting_gate_does_not_stop_or_repin_running_workers`.

## Queue wait

- A job's queue-wait clock starts when its bundles are verified and pinned, not
  at acceptance; upload, fetch and setup do not count. Test:
  `upload_fetch_and_setup_do_not_spend_queue_wait_budget`.
- While the job is queued, its heartbeats carry `queue_wait_ms`. The clock
  advances for capacity and other shared scheduler blockers, but pauses while
  the only blocker is an earlier job that owns the same workspace. If another
  blocker appears, queue time resumes from its accumulated value. Cancellation
  still removes a queued job; the request's execution timeout still starts when
  its command starts. Job status reports the current scheduler blocker in
  `waiting_on`.
  The client keeps a local deadline from `queue_wait_ms` and, when the budget
  runs out, asks the runner to withdraw the job. The runner answers `withdrawn`
  (the job ends as `refused_before_start` / `queue_wait_exceeded`) or `started`
  (too late; the job runs). Only that answer decides. Tests:
  `zero_queue_wait_withdraws_and_cancel_is_idempotent`
  (`crates/motor-module/tests/it/recovery.rs`),
  `only_observed_permit_wait_can_withdraw_not_elapsed_acceptance`,
  `workspace_owner_wait_does_not_spend_queue_wait_budget`,
  `queue_wait_clock_resumes_for_capacity_after_workspace_owner_finishes`,
  `cancelled_workspace_waiter_is_removed_from_the_queue`,
  `jobs_waiting_on_a_workspace_keep_fifo_order`
  (`crates/motor-runner/src/tests.rs`),
  `streamed_elapsed_queue_time_sets_a_local_deadline`
  (`crates/motor-module/src/queue_tests.rs`).
- The budget is the request's `queue_wait_limit_s`, else `ck-motor`'s
  `queue_wait_limit_s` setting (default 300 seconds). Direct `ck motor run`
  jobs get 30 minutes. A gate job does not shorten the budget of jobs behind
  it. Test: `worker_behind_gate_keeps_its_full_queue_wait_budget`.

## Disk admission

The runner checks free space on the filesystem holding its state before it
accepts an upload, and again before setup.

- The hard floor is `serve --free-space-floor-bytes N`, default 128 GiB
  (137438953472 bytes); 0 disables the check. The soft goal is twice the floor.
- Below the soft goal, the runner first cleans up: it evicts workspaces not
  used for 30 minutes, least recently used first, and stale staging. Under the
  hard floor it also evicts recently used workspaces until it reaches the soft
  goal. Tests: `disk_soft_cleanup_evicts_lru_before_the_hard_floor`,
  `disk_soft_pressure_evicts_only_candidates_used_over_30_minutes_ago`,
  `disk_hard_pressure_evicts_recent_candidates_in_lru_order_until_soft_goal`,
  `disk_soft_cleanup_does_nothing_at_twice_the_floor`.
- If space is still under the floor, the job is refused before upload with
  `runner_disk_full`, with `free_bytes` and `floor_bytes` in the refusal. Tests:
  `disk_admission_refuses_typed_when_reflinks_reclaim_too_little`,
  `disk_full_wire_refusal_carries_tag_and_bytes_before_upload`,
  `disk_full_reaches_run_and_prepare_callers_with_known_reason_and_detail`
  (`crates/motor-module/tests/it/recovery.rs`).
- Cleanup never touches running or reserved workspaces, the workspace the
  incoming job needs, or its siblings. It removes only catalogued workspaces
  under the allowed roots (default `/Users`; add with `--workspace-root`). It
  does not follow symlinks or cross mounts. Tests:
  `disk_eviction_skips_reserved_and_running_workspaces`,
  `disk_admission_preserves_incoming_ordinary_workspace_and_sibling`,
  `disk_admission_protects_canonical_alias_through_job_insertion`.
- The compile cache is evicted as a whole, and not while a fill is running;
  single entries are never deleted.

## Retention

- Job records, output and replay history are kept for 24 hours
  (`RETENTION_MS`), counted from the later of the job's end and the time in its
  UUIDv7 ID. After that, `attach` returns `history_expired`. Test:
  `expired_history_never_becomes_before_start_refusal`
  (`crates/motor-module/tests/it/recovery.rs`).
- Snapshot refs, completed transfers and integration tips are pinned for the
  same 24 hours, and while an active transfer or recorded workspace needs them.
  Test: `refused_transfer_survives_maintenance_and_restart_until_retention_expires`.
- A task workspace (the default) is deleted after 24 hours idle. A workspace
  marked `workspace_kind: head` survives idle expiry but can still be evicted
  under disk pressure. Tests: `head_workspace_survives_idle_expiry_and_task_defaults_expire`,
  `head_workspace_is_lru_evictable_outside_task_roots`.
- Output is kept in pages; when old output is evicted, replay starts with a
  `truncated_before_seq` boundary rather than silently skipping. See
  [wire-protocol.md](wire-protocol.md).

## Accounting

Completed Linux jobs record CPU time and wall time from systemd's own summary,
as `reservation_use` with `average_use_ppm` (1,000,000 is 100%). `runner.health`
lists retained jobs that ran over ten minutes below 25% of their reservation.
Missing accounting is absent, never zero, and never delays the terminal record.
Tests: `long_eight_vcpu_job_under_one_core_is_persisted_and_reported`,
`blocked_or_invalid_accounting_never_delays_or_changes_terminal`.

`runner.health` also reports each repository's reserved vCPU-seconds, finished
job count, jobs grouped by requested vCPU size, and queue-wait refusals in the
rolling 24-hour window.
Repositories are grouped by the basename of their main checkout, so its worktrees
share one total; the report is rebuilt from retained job records after restart.
