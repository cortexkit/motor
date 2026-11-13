# Scheduling, capacity and disk admission

This document describes how the runner decides which job runs next, which CPUs
it gets, when a job is refused for capacity or disk, and how long it keeps job
history and workspaces. The code is in `crates/motor-runner/src/engine.rs`,
`cpu.rs` and `disk.rs`. [ARCHITECTURE.md](../ARCHITECTURE.md) shows where these
parts sit; [wire-protocol.md](wire-protocol.md) defines the refusals they
produce.

## vCPU claims

- A job claims exactly its `weight_hint` vCPUs, or 2 when the request has none.
  The command's own flags and environment (such as `-j` or `CARGO_BUILD_JOBS`)
  do not change the claim. Test: `claims_are_exact_and_unsized_jobs_always_claim_two`
  (`crates/motor-runner/src/tests/vcpu.rs`).
- A claim of 0, or larger than the runner's claim limit, is refused before
  start with `runner_full` and a detail naming the limit. The claim limit is
  the host's allocatable CPUs, or `max_weight` when Linux jobs are confined to a
  CPU list (below). Tests:
  `oversized_and_zero_claims_are_refused_before_start_with_limit_detail`,
  `oversized_vcpu_refusal_detail_reaches_the_run_terminal`
  (`crates/motor-module/tests/it/recovery.rs`),
  `linux_cpu_list_refuses_claims_above_max_weight_before_start_without_waiting`
  (`crates/motor-runner/src/tests/vcpu.rs`).
- A claim that fits the claim limit but exceeds `max_weight` (possible only
  without a CPU list) is accepted but waits until it fits; it does not block
  other jobs. Tests: `pinned_job_waits_until_its_exact_claim_fits_max_weight`,
  `without_a_linux_cpu_list_claims_above_max_weight_still_wait`.
- Every new Linux job requires a hosted lease. A broker `no_host_fits` response
  returns a typed refusal; it never sends the job to a direct build-server
  runner. Test: `linux_exec_refusals_and_followups_never_reach_a_direct_runner`.
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

### Confining Linux jobs to a CPU list

`serve --linux-cpus <list>` (for example `12-15,28-31`) places Linux jobs only
on the listed online CPUs, with no core held back for the daemon; a sibling
outside the list is never used. Absent the option, the topology above is
unchanged. With the option:

- The claim limit is `--max-weight`, not the number of listed CPUs. A claim up
  to it is admitted and placed on at most as many CPUs as the list has,
  sharing them as overlapping claims already do; a larger claim is refused
  before start with `runner_full`, whose detail names the limit and the list,
  instead of waiting for capacity that can never come. Tests:
  `linux_cpu_list_confines_placement_and_admits_claims_up_to_max_weight`
  (`crates/motor-runner/src/cpu_tests.rs`),
  `linux_cpu_list_runs_a_long_twelve_vcpu_job_on_the_eight_listed_cpus`,
  `linux_cpu_list_refuses_claims_above_max_weight_before_start_without_waiting`,
  `linux_cpu_list_must_name_online_cpus` (`crates/motor-runner/src/tests/vcpu.rs`).
- Windows avoidance (`--windows-cpus` with `--windows-slot-lock`) still
  applies to the listed CPUs. When the Windows CPUs cover the whole list while
  the Windows slot is active, placement fails open with its rate-limited
  warning, so Linux jobs share the Windows threads and never wait for the slot.
  Tests: `confined_linux_cpus_share_windows_threads_while_the_slot_is_active`,
  `linux_cpu_list_shares_the_windows_threads_while_the_slot_is_active`.
- The option is refused on a VM instance, with an empty list, a CPU that is
  not online, or `--max-weight 0`.

### Memory and the jobs slice

Every Linux job unit runs in `motor-jobs.slice`. A unit's `MemoryMax` is the
claim's `memory_max` clamped to the slice's `MemoryMax`, so a job that
outgrows the slice ends as the slice's OOM kill, reported as before. Only
hosts with an operator-declared `jobs_min_bytes` in
`/etc/motor-host/capacity.json` promise the 16 GiB per-job floor. If that
minimum or the live slice's `MemoryMax` is below the floor, Linux jobs are
refused before start with `workspace_setup_failed` and a detail naming the
repair (raise `jobs_min_bytes` and re-run `provision.py apply-jobs-slice`);
this is a host setup fault, not a full runner. The declared minimum is
separate from the combined Linux and Windows slice maximum. Without a
declaration, Boat, tenant guests and other host-backend instances admit jobs with
limits clamped to the slice, even below 16 GiB. The runner reads both the
slice limit and the declaration at startup and at each maintenance pass (at
least once a minute), not on every admission.
Tests: `the_unit_limit_is_clamped_to_the_slice_and_never_above_it`,
`an_undeclared_boat_sized_slice_admits_eight_vcpus_and_clamps_the_unit_limit`,
`admission_refuses_before_start_when_the_declared_minimum_is_below_the_floor`,
`admission_accepts_when_the_declared_minimum_and_slice_cover_the_floor`,
`maintenance_picks_up_a_changed_ceiling_before_the_next_job_is_decided`,
`maintenance_picks_up_a_changed_declaration_without_changing_the_slice`
(`crates/motor-runner/src/tests/jobs_slice.rs`).

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

Linux gate submissions take host-only leases with
`allow_spill: false` and carry the same `run.gate_job` mark through motor-host. The mark
does not affect broker placement. Each tenant VM has its own runner and queue,
so priority can reorder only that tenant's jobs, never another tenant's jobs.
Explicit repository routes still take precedence. Gates retain their 30-minute
queue budget; `no_host_fits` returns a typed refusal rather than sending the job
to a direct runner.
Tests: `fleet_gate_takes_a_host_only_lease_and_sends_the_gate_mark`,
`linux_exec_refusals_and_followups_never_reach_a_direct_runner`,
`relay_preserves_gate_capability_and_run_mark_for_the_tenant`,
`tenant_gated_run_gets_permits_before_an_older_queued_worker`.

When a head does not fit the free capacity, smaller heads behind it may run in
its place (backfill) for its first 60 seconds. After that, or when the blocked
head is a gate job, backfill stops until it fits. A head blocked by its own
workspace (a checkout in use, a deletion, a sibling conflict) does not stop
others. A head whose last start attempt failed (its setup record or the
catalog could not be saved, say) is queued again and not started before a
retry delay, 1 second doubling to at most 60 seconds per consecutive failure.
During that delay it keeps its place and its claim on capacity, exactly as a
head waiting for permits: a gate or aged head still stops backfill, and when
nothing else (a workspace owner, a sibling checkout) holds it, its
`waiting_on` is `permits`. A head whose earlier start attempt is still being
cleaned up (a Windows VM not yet confirmed stopped) waits like a head blocked
by its own workspace, with `waiting_on` = `workspace setup`, and stops no one:
that cleanup has no upper bound. Tests:
`a_gate_in_its_start_retry_delay_still_fences_capacity`,
`a_gate_with_cleanup_pending_fences_nothing`
(`crates/motor-runner/src/tests/gate_priority.rs`). Running jobs are never
stopped or moved to other CPUs, and no capacity
is permanently reserved. Tests: `capped_head_allows_backfill_only_until_its_grace_expires`
(`crates/motor-runner/src/tests.rs`), `capacity_blocked_gate_still_fences_smaller_workers`,
`workspace_conflicted_gate_does_not_block_independent_workers`,
`waiting_gate_does_not_stop_or_repin_running_workers`.

## Queue wait

- A job's queue-wait clock starts when its bundles are verified and pinned, not
  at acceptance; upload, fetch and setup do not count. Test:
  `upload_fetch_and_setup_do_not_spend_queue_wait_budget`.
  A job that is queued behind a running owner of its workspace starts with its
  clock paused. A job queued again after a failed start resumes its clock at
  that moment, so the failed attempt's setup does not count either. Tests:
  `a_fetched_job_behind_its_workspace_owner_starts_paused`,
  `a_requeued_job_is_not_charged_for_its_failed_setup`
  (`crates/motor-runner/src/tests/state_lock.rs`).
- While the job is queued, its heartbeats carry `queue_wait_ms`. The clock
  advances for capacity and other shared scheduler blockers, but pauses while
  the only blocker is an earlier job that owns the same workspace, or a job in
  another workspace that checks this workspace's directory out as a sibling
  (`waiting_on` = `sibling user of this checkout`). It still advances if the
  job would not fit even once that holder frees its permits. If another
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
  `sibling_user_wait_is_named_with_its_holder_and_does_not_spend_queue_wait_budget`,
  `sibling_user_wait_still_spends_budget_when_capacity_would_also_block`,
  `cancelled_workspace_waiter_is_removed_from_the_queue`,
  `jobs_waiting_on_a_workspace_keep_fifo_order`
  (`crates/motor-runner/src/tests.rs`),
  `streamed_elapsed_queue_time_sets_a_local_deadline`
  (`crates/motor-module/src/queue_tests.rs`).
- An ordinary job's budget is the request's explicit `queue_wait_limit_s`.
  Without one, spill-eligible jobs use `ck-motor`'s `queue_wait_limit_s` setting
  (default 300 seconds); jobs that cannot spill get 30 minutes, capped by their
  execution timeout when present. See the [module configuration](module.md#configuration)
  for the rules and tests. Direct `ck motor run` gate jobs keep their fixed
  30-minute budget. A gate job does not shorten the budget of jobs behind it.
  Test: `worker_behind_gate_keeps_its_full_queue_wait_budget`.

## Fixed-path build bases

Linux repositories can opt into `fixed_path_repositories` (repeated runner
`--fixed-path-repository` options with `--account-subject` and isolation).
The setting is off by default; Windows and macOS are unchanged. Before
deploying the runner on an unprivileged Linux host, reinstall its matching
permissions helper and sudoers rules even with no repository enabled. The
manual reinstall is not covered by a test. Option tests:
`test_fixed_path_disabled_runner_args_golden`,
`test_fixed_path_enabled_record_round_trips_and_renders`
(`script/tenant-provision/test_provision.py`),
`fixed_paths_require_subject_and_isolation_only_when_enabled`
(`crates/motor-runner/src/main.rs`).

Enabled jobs start at `/w/<account>/<repo-key>/<name>`, with the subject's `:`
replaced by `-`, the repository mirror key, and the repository root's basename.
Worktrees share the string, not the checkout. The caller alias remains;
`MOTOR_CALLER_WORKSPACE` gives its path. Streamed stdout/stderr are rewritten to
caller paths. Files the job writes, including logs, test reports and returned
files, and `workspace_changes` are not rewritten and can contain fixed paths.
Tests: `isolated_fixed_paths_share_pwd_but_not_concurrent_checkouts`
(`crates/motor-runner/src/tests/isolated_caller_roots.rs`, checks stream versus
artifact paths), `rewrite_pipe_eof_and_reattach_preserve_bytes_sequences_and_map`
(`crates/motor-runner/src/tests/monitoring.rs`). Returned-file and
`workspace_changes` non-rewriting are not covered by a dedicated test.

Trusted default-branch fills can publish a host- and account-local base at
`<state_dir>/build-bases/<repo-key>/current`, holding `target/` and `base.json`,
independent of the triggering worktree's parent. An eligible empty target gets
a copy-on-write reflink seed. Only that new seed gets the content-based time
step: unchanged tracked files and directories get the fill start time, changed
ones the time step's start time. A kept folder is never time-set. Tests:
`generation_store_ignores_triggering_worktree_parent`
(`crates/motor-runner/src/tests/base_publication.rs`),
`isolated_seed_base_is_warm_and_copy_on_write`,
`isolated_seed_old_dated_change_rebuilds_changed_crate_and_dependents_once`,
`isolated_seed_kept_folder_edit_and_revert_each_rebuild`
(`crates/motor-runner/src/tests/isolated_seed.rs`).
See [compile-cache.md](compile-cache.md#fixed-paths-and-local-build-bases) for
publication, compatibility and recovery, and
[job-isolation.md](job-isolation.md#39-fixed-checkout-paths-and-build-bases) for
the path contract.

An enabled repository with a desired default-branch tip gets a base fill when
the base is missing, at another tip or incompatible, at most once per six hours
since the last base fill started. The catalog persists that bound across
restart and eviction; a due base bypasses descriptor reuse and the
dependency-free shortcut, not fill authority or capacity rules. A tenant
without a remote-cache write key queues no fill. Tests:
`enabling_a_reusable_repository_and_dependency_free_graph_queues_one_fill`,
`source_only_tip_and_eviction_wait_six_hours_across_restart`,
`incompatible_or_unreadable_same_tip_records_refill_once_after_the_interval`,
`selector_is_ordered_and_keyless_tenant_has_no_candidates`
(`crates/motor-runner/src/tests.rs`).

## Disk admission

The runner checks free space on the filesystem holding its state before it
accepts an upload, and again before setup.

Tenant runners and other runners with `--free-space-eviction-bytes` set also
run the ordinary pressure pass every 30 seconds when below their eviction
goal, including while jobs run and during a drain. Running, queued, reserved,
admitting and preparing workspaces and the sibling repositories they check out
stay protected, as do sibling snapshots referenced by those jobs, reservations,
admissions or preparations, with the same quarantine ownership checks as at
admission.
An overlapping pressure pass skips the timer turn. Passes that evict nothing
stay quiet; otherwise one summary records bytes freed, eviction count and free
space before and after. Boat retains its separate reserve-file upkeep; macOS
VM runners retain their fixed thresholds and admission-time cleanup instead
of the timer. Below 40 GiB free, that cleanup deletes idle canonical checkouts,
then kept build disks, then the preparation caches, stopping once 40 GiB is
free. Build folders kept from before build disks go first, then build disks
least recently used first, with the one taking more host disk first among disks
last used at the same moment. A checkout with a job in progress keeps its disk.
Tests: `periodic_disk_pressure_evicts_idle_workspaces_while_a_job_runs_without_admission`,
`periodic_disk_pressure_keeps_reclaiming_during_drain`,
`periodic_disk_pressure_does_nothing_at_or_above_goal`,
`periodic_disk_pressure_skips_a_busy_pass`.

A workspace served through a caller root is checked and evicted on the
filesystem its host directory lives on, not on the caller's `/home`, which the
runner never writes. Admission and eviction decisions are otherwise identical,
and the catalog keeps the caller's `/home` path. See
[job-isolation.md](job-isolation.md#38-caller-root-mapping).

Linux host workspaces also retain their checkouts while trimming Cargo outputs:
after a job becomes idle, `--build-folder-cap-bytes` (default 64 GiB) drops only
incremental directories when allocated `target/` bytes exceed the cap. Periodic
maintenance sweeps each idle workspace at most once per day per runner lifetime.
It removes whole name/hash units only when every known output's atime is older
than 48 hours, keeping a full extra day beyond relatime's update interval.
Unknown files and symlinks are left alone; incremental crate directories are
aged independently. Boat skips atime sweeping because its disk uses noatime,
and retains its admission-time whole-workspace sweep. VM-backed jobs keep their
backend-specific disk policies.

The runner caches allocated workspace, target and incremental sizes in memory,
with a measurement time and completeness flag. Read-only walks refresh them
when a workspace becomes idle and populate missing entries lazily for ranking;
a restart starts with an
empty cache. Unreadable subtrees mark the estimate incomplete and log one
warning per walk. Read-only measurement never claims a workspace or changes
permissions.
Build-folder cleanup scans before claiming and repairs ownership only when it
has selected outputs to delete: repair transfers the target to the runner, so
the next job must repair it back to the job account. An incomplete estimate is
the exception: the guard, daily sweep and incremental pressure pass claim and
repair that workspace, then measure again before deciding what to remove. This
keeps jobs that narrow their target's permissions from hiding their disk usage.
The whole-workspace pass tries eligible complete estimates first, then repairs
unreadable workspaces one at a time, oldest first, and reranks their complete
sizes. It stops before repairing another workspace if the free-space goal is met.
Tests:
`disk_size_guard_drops_only_incremental_and_defers_queued_workspace`,
`disk_stale_unit_maintenance_deletes_whole_units_and_runs_once_daily`,
`cargo_units_group_hashes_outputs_and_scripts_without_following_symlinks`.

The build-folder cap and stale-unit sweep never touch a published base, nor
does whole-workspace eviction: bases live in the separate state-directory
store. Seeded targets are measured like other targets; allocated bytes count
shared reflink extents in full. Going over the build-folder cap drops only the
workspace's incremental directories, not its base. Test:
`cap_stale_sweep_and_whole_workspace_pass_leave_base_untouched`
(`crates/motor-runner/src/tests.rs`).

- The hard floor is `serve --free-space-floor-bytes N`, default 128 GiB
  (137438953472 bytes); 0 disables the check. The soft goal defaults to twice the
  floor; `--free-space-eviction-bytes N` explicitly sets a reclaim goal above it.
- Below the soft goal, cleanup has four passes: stale Cargo units, incremental
  state (largest cached incremental size first), whole workspaces and staging
  ordered by allocated bytes times idle milliseconds, descending, then published
  build bases (oldest last use first, skipping leased bases). Workspace sizes
  come from the in-memory cache; staging sizes are measured on each pass.
  Unreferenced sibling snapshots retain priority in the whole-tree pass.
  Each pass stops once the free-space
  probe reaches the soft goal; allocated bytes can double-count reflink extents.
  Workspaces used in the last 30 minutes remain protected above the hard floor;
  below it recent idle workspaces are eligible too. Tests:
  `disk_pressure_stale_pass_reaches_goal_without_dropping_incremental`,
  `disk_pressure_incremental_pass_prefers_largest_and_retains_workspaces`,
  `disk_eviction_bytes_times_idle_prefers_huge_workspace_over_older_small_ones`,
  `disk_soft_cleanup_evicts_lru_before_the_hard_floor`,
  `disk_soft_pressure_evicts_only_candidates_used_over_30_minutes_ago`,
  `disk_hard_pressure_evicts_recent_candidates_in_lru_order_until_soft_goal`,
  `disk_soft_cleanup_does_nothing_at_twice_the_floor`.
- The fourth pass runs before a `runner_disk_full` refusal, so reclaiming a
  base can admit the job when it reaches the goal. A seed lease protects the
  generation even if eviction already selected it; after release, a missing
  base is a normal cold-build fallback. Tests:
  `fourth_pass_evicts_oldest_base_after_workspaces_and_staging_before_refusal`,
  `seed_pin_racing_eviction_keeps_current_and_allows_cold_fallback_after_release`
  (`crates/motor-runner/src/tests.rs`).
- In a job VM the disk holds only the incoming job's own checkout, which the
  passes above protect. So whenever free space is still under the soft goal,
  admission deletes that checkout's incremental state and then, if needed,
  its whole `target`, stopping at the goal; the checkout itself stays. This
  keeps a job from starting with only the floor left and failing mid-build.
  Shared tenant VMs don't do this. Tests:
  `job_vm_admission_clears_its_own_build_outputs_before_refusing_for_disk`,
  `job_vm_admission_clears_its_own_incremental_caches_under_the_eviction_goal`
  (`crates/motor-runner/src/tests/disk_pressure.rs`).
- If space is still under the floor, the job is refused before upload with
  `runner_disk_full`, with `free_bytes` and `floor_bytes` in the refusal. Tests:
  `disk_admission_refuses_typed_when_reflinks_reclaim_too_little`,
  `disk_full_wire_refusal_carries_tag_and_bytes_before_upload`,
  `disk_full_reaches_run_callers_with_known_reason_and_detail`
  (`crates/motor-module/tests/it/recovery.rs`).
- Cleanup never touches running or reserved workspaces, the workspace the
  incoming job needs, or its siblings. It removes only catalogued workspaces
  under the allowed roots (default `/Users`; add with `--workspace-root`, and a
  `--caller-root` adds its host directory too). It does not follow symlinks or
  cross mounts. Tests:
  `disk_eviction_skips_reserved_and_running_workspaces`,
  `disk_admission_preserves_incoming_ordinary_workspace_and_sibling`,
  `disk_admission_protects_canonical_alias_through_job_insertion`.
- A workspace served through a caller root counts like any other. Disk
  accounting, the free-space floor and the size/idle order all use the host directory
  that actually holds the trees, while the catalog keeps the caller's `/home`
  path. Eviction renames such a tree into the host directory's own trash, never
  the state directory's. Tests:
  `mapped_expiry_and_pressure_use_host_paths_and_private_trash`,
  `mapped_pressure_protects_active_host_claims_and_collects_fresh_trash_garbage`
  (`crates/motor-runner/src/tests/caller_trash.rs`); see
  [job-isolation.md](job-isolation.md#38-caller-root-mapping).
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
as `reservation_use` with `average_use_ppm` (1,000,000 is 100%). The runner's
status-protocol health payload
lists retained jobs that ran over ten minutes below 25% of their reservation.
Missing accounting is absent, never zero, and never delays the terminal record.
Tests: `long_eight_vcpu_job_under_one_core_is_persisted_and_reported`,
`blocked_or_invalid_accounting_never_delays_or_changes_terminal`.

That payload also reports each repository's reserved vCPU-seconds, finished
job count, jobs grouped by requested vCPU size, and queue-wait refusals in the
rolling 24-hour window.
Repositories are grouped by the basename of their main checkout, so its worktrees
share one total; the report is rebuilt from retained job records after restart.

The report adds optional host load, logical CPU threads, available memory,
held/capped vCPU permits (including workspace setup), and hourly queue-wait
percentiles and refusals. Disk free bytes and pending queue depth retain their
existing fields. Hourly counts use sixty minute buckets and reset on restart;
the nearest-rank p50/p90 use the same capacity wait as `started`, not time behind
another job in the same checkout. Empty windows report zero. Active jobs and
underused reservations are indexed on record saves, so probes do not scan all
retained records under the state lock. Tests:
`host_sample_parses_proc_load_threads_and_available_bytes`,
`health_reports_reserved_permits_pending_jobs_and_optional_wire_fields`,
`queue_wait_percentiles_and_refusals_expire_with_hourly_buckets`,
`started_is_recorded_only_after_setup_with_capacity_wait`,
`workspace_owner_wait_does_not_spend_queue_wait_budget`.
