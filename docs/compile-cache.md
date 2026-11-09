# Compile cache and warm builds

Linux jobs share one compiler cache per runner instance, so a job does not
recompile dependencies that another job, or a background build, already
compiled. Jobs only read the cache. A separate trusted account fills it from the
repository's default-branch tip, never from a submitted branch. This document
covers who reads and writes the cache, what goes into a fill, and the local
build bases that can seed private build folders.
[ARCHITECTURE.md](../ARCHITECTURE.md) shows where the cache sits;
[job-isolation.md](job-isolation.md) describes the reader's sandbox.

The Windows backend has its own per-workspace cache disks, described in
[windows-jobs.md](windows-jobs.md#cache-disks).

## Leased runners

Boat profiles never queue background cache fills or warm builds, and a tenant
VM queues none unless its provisioning holds a remote cache write key. An
idle-started fill can outlive the idle period, compete for leased CPU time, or
hold a setup lock when the next job arrives. Boat avoids that because it only
reads the cache. A tenant VM given a write key accepts the delay: it fills its
own cache with its own capacity, one fill at a time
and only on unallocated permits. Its runner reports the running fill's permits
as `fill_permits` in health, and motor-host subtracts them from that tenant's
advertised `free_vcpus` while the report is fresh, so the broker places new
jobs elsewhere meanwhile. The report is tenant input, so it never raises the
VM's CPU quota or changes usage and billing: a fill runs within the CPU its
tenant's leases already hold, or on the idle quota of one vCPU. A tenant
without a write key logs once that it does
not fill. Existing cache entries remain readable, and foreground jobs compile
misses themselves. Restart still stops old writer processes, but does not
requeue their saved requests in a mode that may not fill. Tests:
`keyless_tenant_vm_fill_cannot_delay_a_leased_job`,
`boat_fill_cannot_delay_a_leased_job_even_with_a_write_key`,
`leased_modes_do_not_requeue_persisted_fills`,
`keyed_tenant_vm_schedules_a_fill_and_reports_its_permits`,
`keyless_tenant_vm_logs_once_and_runs_no_fill`
(`crates/motor-runner/src/tests/dependency_writer.rs`) and
`a_tenant_fill_report_lowers_only_its_free_vcpus_and_only_while_fresh`
(`crates/motor-host/tests/it/capacity.rs`),
`a_forged_fill_report_grants_no_cpu_waives_no_usage_and_lowers_only_its_own_capacity`
and `a_job_beside_its_tenants_fill_keeps_its_memory_and_egress`
(`crates/motor-host/tests/it/billing.rs`).

ck-motor offers default-branch tips only where the runner may fill. Hosted
channels to a registered host's tenant runner (a `first_party` placement)
advertise the fill capability in Hello and send the resolved integration tip.
Boat spills omit both.
Tests:
`hosted_hello_advertises_fill_support_only_to_registered_host_tenants`,
`hello_advertises_default_branch_tip_on_the_wire`,
`direct_boat_submission_omits_a_resolved_fill_tip`,
`direct_first_party_submission_sends_the_resolved_fill_tip`,
`spill_after_permit_wait_preserves_original_stream_output_and_exit`
(`crates/motor-module/src/transport.rs`, `crates/motor-module/src/queue_tests.rs`).

Before `Started`, INFO logs identify the job's materialization start and finish,
each tree-access, dependency-mirror, and build-wrapper lock wait (start and elapsed
milliseconds), and the hand-off to execution. Wait messages do not include
checkout paths or command/environment contents. A start without a matching finish
names the last phase entered, but can also mean the daemon stopped before logging
completion; queue-wait time alone excludes materialization.
Test: `materialization_logs_identity_waits_and_failure_without_paths`
(`crates/motor-runner/src/setup_log.rs`).

## Parts

| Part | Where | Owner |
| --- | --- | --- |
| Shared cache | `<state-root>/<instance>/sccache/fleet` | `motor-cache` |
| Reader view | `/motor-cache` inside the job sandbox, read-only | Bound into `motor-job` sandboxes |
| Per-workspace build output | `/run/motor/target` inside the sandbox | The job; never shared between workspaces |
| Compiler wrapper | `crates/motor-runner/src/sccache-wrapper.sh`, installed by the runner | Runner |
| Cache key inputs | `crates/motor-runner/src/compile_cache.rs` | Runner |
| Fill identity | `crates/motor-runner/src/writer/fill.rs` | Runner |

`serve --sccache-size-gib N` caps the cache; the default is 200 GiB
(`crates/motor-runner/src/main.rs`, test
`sccache_size_defaults_to_200_gib_and_accepts_a_cap`).

Readers and the writer use sccache 0.18.0 built with
`script/runner/patches/sccache-0.18.0-read-only-hits.patch`. Stock 0.18.0
updates an entry's file times before every read, which fails on a read-only
mount and turns every lookup into a miss. `script/runner/build-sccache.sh`
builds the patched binary; see [runner-install.md](runner-install.md).

## Reading

A job that compiles Rust gets the read-only cache and a wrapper that sends
`rustc` through sccache. The job cannot write entries, including through build
scripts or procedural macros, because the cache belongs to another account and
the mount is read-only.

- A missing or evicted cache does not stop a job: the wrapper falls back to the
  plain compiler. Test: `isolated_public_reader_missing_cache_eviction_and_server_fallback`
  (`crates/motor-runner/tests/it/reader_environment.rs`, operator-only).
- A job that does not build Rust gets no sccache environment. Test:
  `isolated_public_non_rust_job_has_no_sccache_environment` (operator-only).
- A `build.rustc-wrapper` set in the repository's Cargo configuration takes
  precedence over the runner's wrapper; an empty value turns wrapping off.
  Tests: `isolated_public_reader_wrapper_precedence_matrix`
  (operator-only), `cargo_wrapper_configuration_obeys_ancestor_and_legacy_precedence`
  (`crates/motor-runner/src/compile_cache.rs`).
- A genuine compiler failure is not retried; an unavailable cache server runs
  the compiler once. Tests: `genuine_cached_compiler_failure_is_not_retried`,
  `genuine_uncached_compiler_failure_is_not_retried`,
  `unavailable_cache_server_runs_compiler_once`.

### Why readers and the writer must match

sccache keys a Rust compile on, among other things, the compiler arguments, the
working directory, and Cargo's environment. Registry and Git dependencies
compile from their directory under the shared `CARGO_HOME`, so the writer and
every reader must see the same `CARGO_HOME` path and the same Cargo variables.
These include `CARGO_TARGET_DIR=/run/motor/target` and
`CARGO_BUILD_BUILD_DIR=/run/motor/target/bd/{workspace-path-hash}`. Cargo expands
the build-folder template; the variable's literal value is the same in fills
and jobs.
Otherwise identical sources produce unrelated entries and every lookup misses.
Test: `reader_and_writer_have_identical_hashed_cargo_inputs`.

Linux fills and isolated Rust jobs also share
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` and
`CARGO_PROFILE_TEST_DEBUG=line-tables-only`. Line-only debug info keeps file and
line numbers in panics and backtraces, but omits debugger variable and type
detail. A job wanting full debug info for a debugger or a tool that reads
variables sets `CARGO_PROFILE_DEV_DEBUG=true` (or its repository's profile sets
`debug`). macOS and Windows jobs are unchanged. Tests:
`linux_debug_defaults_keep_line_tables`,
`isolated_jobs_preserve_submitted_profile_debug`.

The shared decision injects neither variable if either is already in the
submitted environment, with any value, or if the checkout's root `Cargo.toml`,
`.cargo/config.toml` or legacy `.cargo/config` sets `profile.dev.debug` or
`profile.test.debug`. Package-level overrides still apply on top of the default
and do not opt out. Unparsable files log a warning and count as unset. Tests:
`either_submitted_debug_variable_suppresses_both_defaults`,
`root_manifest_profile_debug_suppresses_both_defaults`,
`cargo_config_toml_profile_debug_suppresses_both_defaults`,
`legacy_cargo_config_profile_debug_suppresses_both_defaults`,
`package_debug_overrides_do_not_suppress_profile_defaults`,
`unparsable_cargo_files_keep_debug_defaults`.

Only regular files of at most 1 MiB from the job or fill snapshot tree are
inspected; symlinks and oversized files count as unset. Tests:
`symlinked_cargo_profiles_keep_defaults_without_following_targets`,
`symlinked_cargo_directory_keeps_defaults_without_following_targets`,
`oversized_cargo_profiles_keep_defaults`.

Fills read those same files at their checked-out tip; they do not inherit a
job's submitted environment. With the same inputs, fills and readers must get
byte-identical `CARGO_*` values or sccache loses every hit. Changing this default
costs one rebuild of every warm workspace and one round of cache refills. Test:
`isolated_fill_and_job_have_identical_hashed_cargo_inputs`.

Incremental compilation: the writer builds local crates incrementally, and the
wrapper strips `CARGO_INCREMENTAL` and `CARGO_BUILD_INCREMENTAL` before
sccache sees them, because sccache refuses to cache when either is `1`. Test:
`incremental_variables_are_stripped_for_cached_compiles_only`.

## Filling

A fill runs the repository's warm command, as `motor-cache`, against the
repository's default-branch tip held in the runner's Git mirror.

### Verifying the fill tip

`ck-motor` selects a local candidate using the branch named by `origin/HEAD`,
or local `main`, then `master`; it never falls back to the submitted `HEAD`.
Those refs are hints, not authority. On a verification-cache miss it runs
`git ls-remote --symref origin HEAD` and requires the remote default branch's
name **and** commit to equal the candidate. A local remote-tracking ref is not
proof: an agent can change it with `update-ref`. Checking the branch name also
prevents a forged local `origin/HEAD` from selecting a remote feature branch.
Tests (`crates/motor-module/src/default_tip.rs`):
`local_tip_ahead_of_remote_has_no_tip`, `equal_remote_default_branch_returns_tip`,
`forged_origin_head_cannot_authorize_a_remote_feature_branch`.

The check uses the configured `origin` fetch URL, not a pinned GitHub URL.
Rewriting that URL can point the check at a different repository; this more
visible configuration change is an accepted limit. Code merged to each
repository's remote default branch is still trusted to write the shared cache.
The module and its local store are trusted too; the runner relies on the
client's report, not an independent remote attestation.

The module's store keeps only the last successfully verified URL, branch and
commit for each canonical Git common directory, shared across linked
worktrees. At most 256 repositories are retained, oldest verification first,
for at most 24 hours. Reads do not extend that age. An unchanged verified tip
needs no network round trip, even after module restart; a changed tip, branch
or effective origin URL requires another check. A cached success proves that
the commit was on the remote default branch when checked, not that it remains
the current tip after a force-push. Failures never become successful entries.
Tests: `cache_hit_avoids_second_ls_remote`, `changed_tip_reverifies`,
`changed_origin_url_reverifies`, `successful_verification_survives_storage_reopen`,
`mismatch_is_never_cached_as_success_and_recovers_after_ttl`,
`verified_tip_age_is_bounded_and_future_timestamps_are_refused`,
`verified_tips_keep_only_256_repositories_and_one_tip_each`.

Failures have a separate, memory-only negative cache. A mismatch, authentication
failure, network failure or timeout suppresses another query for the same
repository, effective origin URL, branch and local commit for 60 seconds. A hit
returns no fill tip and emits no repeated warning; it does not extend the TTL.
A changed key checks immediately, and expiration permits another check and
warning if the failure continues. This avoids charging every gate job for a
local merge that has not been pushed yet, or for an unavailable remote. At most
256 failed keys are retained, oldest failure first. Negative entries disappear
on module restart and are never persisted or treated as successful verification.
Tests: `negative_cache_skips_second_query_and_warning_within_ttl`,
`negative_cache_rechecks_after_ttl`,
`negative_cache_changed_local_commit_rechecks_immediately`,
`negative_cache_changed_origin_url_rechecks_immediately`,
`negative_cache_is_not_persisted`,
`negative_cache_keys_all_verification_inputs_and_is_bounded`.

Git inherits the module's `HOME` and `PATH`, so existing credential helpers and
SSH credentials remain available; no runner-vault credential is lent to Git.
The child sets `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`,
`GIT_SSH_COMMAND="ssh -o BatchMode=yes"`, disables askpass, and has no stdin.
The query has a five-second hard timeout that kills Git's process group,
including credential and SSH helpers. A failed check emits one warning with
`repository`, `category` (`auth`, `network`, `timeout` or `mismatch`), `reason`
(the first line of Git's stderr when present), and `elapsed_ms`. A local cache
write error uses category `cache` and also skips the fill. No failure blocks
the submitted job or prepare request; an empty integration tip skips fills.
Tests: `child_is_noninteractive_and_preserves_home_and_path`,
`auth_failure_is_negative_cached_and_logs_first_stderr_line_once`,
`hung_remote_times_out_and_kills_helpers`, `unreachable_remote_has_no_tip`,
`current_client_unverified_default_tip_runs_without_fills`.

### What a fill builds

The warm command comes from the tip's root `Cargo.toml`:

```toml
[workspace.metadata.motor]
warm-commands = ["cargo nextest run --workspace --no-run"]
```

`[package.metadata.motor]` works for a single-package repository. The commands
run in order, joined with `&&`. A Cargo project that declares nothing gets
`cargo build --workspace --all-targets`; a tree without a root `Cargo.toml` has
nothing to warm. No `serve` option overrides the warm command;
`Config::base_commands` overrides it for code that embeds the runner, and for
tests.

### Inputs

- The tip only. A fill never uses the submitted snapshot, its environment, or
  sibling checkouts. Test: `writer_uses_only_integration_tip_not_branch_lockfile_or_siblings`.
- Trusted preparation fetches the tip's locked sources first. The writer then
  runs offline; an offline preflight (`crates/motor-runner/src/writer/preflight.py`)
  refuses sibling path dependencies and missing registry sources before any
  compiler runs. Test: `offline_preflight_rejects_sibling_paths_and_missing_registry_sources_without_compiling`.
- A fill that changes the checked-in lockfile fails, and its scratch is
  removed. Test: `writer_lockfile_mutation_fails_and_scratch_is_removed`.

### When fills run

- A fill's identity is its descriptor: a hash of the toolchain, the warm
  command, and every `Cargo.toml`, `Cargo.lock`, `rust-toolchain*` and
  `.cargo/config*` path and blob in the tip. A new tip with the same descriptor
  reuses the previous fill unless a local build base is due (see below). Tests:
  `descriptor_reuses_source_only_tips_and_refills_manifest_and_config_changes`,
  `descriptor_changes_for_toolchain_and_command_without_lockfile_changes`.
- At most one fill runs per instance (one writer slot), with a fixed 4 permits
  (fewer on a smaller host). It does not wait for queued foreground jobs:
  under steady load the queue never empties, and an emptied cache would never
  be filled again. On a host so small that the fill would take over half of
  the permits, it still waits until no foreground job is queued. A fill that
  may start is tried before every queued foreground job except gates, takes
  only permits that are free, and holds back later jobs while it waits for
  them, as a job past its backfill grace does. It never takes permits already
  held by a running or starting job. The next fill is for the repository whose jobs arrived most
  recently. Tests:
  `writer_is_a_singleton_that_waits_for_free_permits_on_large_and_small_hosts`,
  `one_fill_reaches_free_permits_past_a_queued_job_that_does_not_fit`.
- Reader misses can request one refill per repository, tip and descriptor, and
  only when the submitted snapshot's descriptor matches the tip's. The bound
  survives a runner restart. A branch that changes dependencies never triggers
  a fill. Tests: `cacheable_reader_misses_refill_the_successful_descriptor`,
  `miss_refill_bound_survives_runner_restart`,
  `branch_only_dependency_misses_never_queue_a_trusted_refill`.
- A failed descriptor is not retried until it changes or the runner restarts,
  except when a local build base is due.
  Test: `failed_descriptor_is_suppressed_until_change_or_restart`.

### After a fill

The runner records the files the writer created (with `inotify`, without
walking existing entries) and repairs their permissions so readers can open
them. Overflowing the watch, or an unreadable entry, fails the fill rather than
advertising it. Tests: `cache_fill_repairs_only_changed_entries_and_never_follows_symlinks`,
`cache_fill_tracking_and_repair_fail_closed`. The writer's scratch target is
discarded unless the repository is enabled for local build bases. An enabled
fill may publish a protected base for copy-on-write seeding, never a target
that jobs write in common; see below.

### Without a cache account

If `motor-cache` is missing or invalid, fills and cache tracking are disabled;
foreground jobs still run. Test: `unavailable_writer_account_disables_fills_and_cache_tracking`.

## Fixed paths and local build bases

### Enablement and paths

This separate, Linux-only optimization is off by default. Provisioning's
`fixed_path_repositories` list enables exact caller repository roots, or `*`
alone. It renders repeated `--fixed-path-repository` options and
`--account-subject`; roots must be absolute, distinct and whitespace-free, and
the runner requires a valid subject and isolation when enabled. A record with
no setting preserves its arguments. Tests:
`test_fixed_path_disabled_runner_args_golden`,
`test_fixed_path_enabled_record_round_trips_and_renders`,
`test_fixed_path_records_refuse_malformed_roots`
(`script/tenant-provision/test_provision.py`),
`fixed_paths_require_subject_and_isolation_only_when_enabled`
(`crates/motor-runner/src/main.rs`).

Before deploying this runner on an unprivileged Linux host, reinstall its
matching permissions helper and sudoers rules, even with the setting empty:
base link checking and removal use `check-staging-links` and `remove-staging`.
Root runners perform them in-process. The reinstall itself is not covered by
a test; the installed contract and deployment refusal are checked by
`install_contract_requires_matching_helper_and_both_staging_grants`
(`crates/motor-runner/src/permissions/tests.rs`) and
`test_missing_helper_grant_aborts_before_drain_or_stop`
(`script/runner/test_deploy.py`). See
[job-isolation.md](job-isolation.md#39-fixed-checkout-paths-and-build-bases).

Enabled jobs and fills run at `/w/<account>/<repo-key>/<name>`. The account is
the subject with `:` replaced by `-`; the key is the repository's mirror key
and the name is its root basename, never the worktree name. Each job still has
its own checkout and target, with the caller workspace as another alias.
`MOTOR_CALLER_WORKSPACE` gives the job that caller path. Streamed stdout/stderr
replace fixed paths with caller paths, but files the job writes (including
logs, test reports and returned files) and `workspace_changes` are not
rewritten and can contain fixed paths. Tests:
`fixed_paths_use_repository_identity_not_worktree_or_cwd`
(`crates/motor-runner/src/caller_path.rs`),
`isolated_fixed_paths_share_pwd_but_not_concurrent_checkouts`
(`crates/motor-runner/src/tests/isolated_caller_roots.rs`),
`rewrite_pipe_eof_and_reattach_preserve_bytes_sequences_and_map`
(`crates/motor-runner/src/tests/monitoring.rs`). Returned-file and
`workspace_changes` non-rewriting have no dedicated test.

Basename collisions with siblings use the legacy path layout, without rewrite
or seeding. Build-directory preparation keeps dep-info rooted at the fixed
checkout and sibling aliases only when the fixed plan exists; otherwise it
uses the legacy caller roots. Tests:
`prepare_keeps_fixed_checkout_and_sibling_dep_info`,
`disabled_and_colliding_jobs_use_only_legacy_prepare_roots`
(`crates/motor-runner/src/cargo_build_dir.rs`),
`rewrite_launch_map_pairs_checkout_and_siblings_and_omits_fallback`
(`crates/motor-runner/src/tests/monitoring.rs`). Disabled systemd unit arguments
remain byte-identical: `job_unit_commands_match_the_pinned_argv`
(`crates/motor-runner/src/isolation/command_golden_tests.rs`).

### Store and publication

The store is `<state_dir>/build-bases/<repo-key>/`, independent of which
worktree triggered a fill. It holds `current`, `staging-<fill-job-id>` and
`retired-<UUID>` generations. Each generation holds `target/`; published
generations also hold `base.json`, outside every job bind. Format 1 records the
base commit and tree, toolchain identity, environment fingerprint, fixed path,
fill start time and newest target mtime. Launch inputs are saved before the
fill runs, so a fill surviving restart publishes with those same inputs.
Tests: `generation_store_ignores_triggering_worktree_parent`,
`startup_keeps_live_fill_generation_and_recorded_launch_inputs_and_deletes_retired`
(`crates/motor-runner/src/tests/base_publication.rs`).

Only a fill that ran, exited zero, was not cancelled or timed out and left a
nonempty `bd/` may publish. Its checkout must still match the base tree, with
no tracked inode change time later than the pre-launch reference. Changing
then restoring a source, or creating an untracked root `Cargo.lock`, refuses
publication. The previous base stays in place and the reason is logged. Tests:
`failed_cancelled_timed_out_and_unrun_fills_keep_current_and_log_each_reason`,
`tracked_change_and_restore_and_untracked_root_lock_are_refused_before_metadata_repair`
(`crates/motor-runner/src/tests/base_publication.rs`).

Publication checks for links outside the generation before changing any
ownership, mode or ACL. The runner then owns the generation, removes job write
access, creates `base.json` exclusively without following links, and atomically
renames the generation to `current`. Replacement retires the old generation;
an active seed lease delays its deletion. No job writes the base. Tests:
`publication_is_exclusive_no_follow_and_atomically_replaces_the_generation`
(`crates/motor-runner/src/tests/base_publication.rs`),
`current_exchange_before_and_during_clone_stays_on_validated_generation`
(`crates/motor-runner/src/seed.rs`),
`isolated_base_fill_publishes_nested_uplift_links_and_replaces_current`,
`isolated_base_fill_external_home_hardlink_preserves_outside_metadata`
(`crates/motor-runner/src/tests/isolated_targets.rs`),
`isolated_seed_base_is_warm_and_copy_on_write`
(`crates/motor-runner/src/tests/isolated_seed.rs`).

### Refresh

An enabled repository's base is due when it is missing (including after
eviction), at a different default-branch tip, unreadable, or incompatible in
format, fixed path, toolchain or environment, and no base fill started in the
last six hours. The last start is persisted in the catalog, so restart does
not reset the bound. The scheduler revisits enabled repositories with a desired
tip without needing another job request. A due base bypasses prior descriptor
success or failure records, the dependency-free shortcut and remote-cache
publication retry delays. It does not bypass repository authorization,
workspace overlap checks, cleanup of an earlier fill, or the limits of one
live fill per repository and queue depth.
Tests: `enabling_a_reusable_repository_and_dependency_free_graph_queues_one_fill`,
`source_only_tip_and_eviction_wait_six_hours_across_restart`,
`incompatible_or_unreadable_same_tip_records_refill_once_after_the_interval`
(`crates/motor-runner/src/tests.rs`).

Base fills still require writer authority: a tenant without a remote-cache
write key queues none, even when fixed paths and output rewriting are enabled.
Test: `selector_is_ordered_and_keyless_tenant_has_no_candidates`
(`crates/motor-runner/src/tests.rs`). A stale base costs rebuilds, not freshness:
the seed's content comparison and Cargo's own fingerprints still apply.

### Seeding and compatibility

Seeding requires an enabled, non-fallback job with no siblings, an empty
`target/`, and no entries outside the verified tree except `.git` and `target/`.
Ignored leftovers, including previously prepared `node_modules`, prevent a
seed. A symlink added, removed or retargeted relative to the base also prevents
it. The base must be on the same device, at the same fixed path, with format 1
and equal toolchain identity and environment fingerprint. An absent or
incompatible base leaves the job to build normally. Tests:
`eligibility_refuses_incompatible_records_and_leftovers`,
`symlink_add_remove_and_retarget_refuse_seed`,
`different_base_device_is_ineligible`
(`crates/motor-runner/src/seed.rs`),
`isolated_seed_ignored_watched_file_and_clean_keep_next_job_unseeded`
(`crates/motor-runner/src/tests/isolated_seed.rs`).

Toolchain identity covers the runner's `rustc -vV`, the snapshot's root
`rust-toolchain` and `rust-toolchain.toml` blob ids (including absence), and
runner-provided wrapper inputs. The environment fingerprint covers effective
`CARGO_*`, `RUSTFLAGS`, `RUSTDOCFLAGS` and `RUSTC_WRAPPER`, excluding
`CARGO_BUILD_JOBS` and `CARGO_INCREMENTAL`. Absent and empty variables differ;
the encoding preserves variable names and values. Concurrency changes alone
(`CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `NEXTEST_TEST_THREADS`) and the writer's
`CARGO_INCREMENTAL` do not prevent seeding. Tests:
`identity_records_snapshot_root_blobs_absence_and_runner_verbose_version`,
`identity_is_stable_before_supervisor_build_and_after_rebuild`,
`sccache_identity_hashes_embedded_script_and_backend_not_generated_tools`
(`crates/motor-runner/src/build_base.rs`),
`environment_fingerprint_uses_sorted_length_framed_names_and_values`
(`crates/motor-runner/src/compile_cache.rs`),
`concurrency_and_incremental_environment_differences_still_seed`
(`crates/motor-runner/src/seed.rs`).

The job's effective launch environment must still match the seed's identity.
For example, a tenant job losing its remote-cache socket cannot keep a seed
prepared for that wrapper environment. Test:
`remote_socket_loss_after_seed_launches_cold_and_matching_environment_keeps_seed`
(`crates/motor-runner/src/seed.rs`). A command overriding `CARGO_TARGET_DIR` or
`CARGO_BUILD_BUILD_DIR` bypasses the seeded layout and has no reuse guarantee;
that override is not covered by a dedicated test.

### Content-based time step

After cloning, only a folder seeded in this setup gets its tracked file and
directory times set. A blob or mode change, a changed directory tree id, or a
path absent from the base gets the time step's start time; unchanged paths get
the base's fill start time. The comparison uses the verified snapshot tree,
including uncommitted content, never author or committer dates. Dating changed
directories makes a build script watching `rerun-if-changed=<dir>` detect a
deletion even when no remaining file is newer. Tests:
`changed_mode_added_deleted_and_renamed_paths_stamp_their_trees_not_dates`
(`crates/motor-runner/src/seed_times.rs`),
`isolated_seed_old_dated_change_rebuilds_changed_crate_and_dependents_once`,
`isolated_seed_mode_only_rebuilds_changed_crate_and_dependents_once`,
`isolated_seed_added_file_rebuilds_changed_crate_and_dependents_once`,
`isolated_seed_deleted_file_rebuilds_changed_crate_and_dependents_once`,
`isolated_seed_deleted_watched_file_in_reused_workspace_reruns_script_once`
(`crates/motor-runner/src/tests/isolated_seed.rs`).

The Cargo reuse guarantee is limited to the units the fill warmed. At the base
tree, the tested warm command compiles no local crate on the first seeded
build. In the changed-file fixtures, changed crates and their dependents
rebuild once and subsequent builds compile nothing; disabling the time step
fails that assertion. A kept folder is never time-set, so edit/build/revert
still rebuilds. Tests: `isolated_seed_base_is_warm_and_copy_on_write`,
`isolated_seed_without_time_step_fails_the_changed_crate_assertion`,
`isolated_seed_kept_folder_edit_and_revert_each_rebuild`
(`crates/motor-runner/src/tests/isolated_seed.rs`). This is not a guarantee that
every possible job command is warm or that non-Cargo outputs are validated.

The no-follow walk sets a tracked symlink's own time, never its target's; a
link planted after verification is refused. The step refuses a current time
not strictly later than the base's newest target mtime. Launch-time source
rematerialization preserves the seeded file and parent-directory times.
Tests: `unchanged_root_and_tracked_symlink_receive_old_time_without_touching_target`,
`file_symlink_planted_after_verification_is_refused_not_followed`,
`time_not_strictly_after_newest_base_mtime_is_refused_before_any_stamp`
(`crates/motor-runner/src/seed_times.rs`),
`isolated_seed_launch_rematerialization_preserves_file_and_watched_directory_times`
(`crates/motor-runner/src/tests/isolated_seed.rs`).

### Failure and recovery

Seeding pins one opened generation so the record and cloned files cannot come
from different replacements. After writing a durable seed intent, the runner
creates a staging directory and records its and the empty target's inodes in
the intent. It then reflinks into staging, exchanges it with the empty
`target/`, repairs ownership and sets times. Only then is the intent
removed and the folder becomes an ordinary kept folder. A failure (including
EXDEV, EOPNOTSUPP, a partial clone or time-step failure) settles the intent,
empties the seeded target and logs one warning; there is no full-copy or
hard-link fallback. The job builds normally. Recovery removes only entries
authorized by the intent, never arbitrary `.motor-seed-*` files. Tests:
`current_exchange_before_and_during_clone_stays_on_validated_generation`,
`failures_settle_once_warn_once_and_job_builds_without_seed`,
`killed_setups_recover_and_committed_seed_remains_kept`
(`crates/motor-runner/src/seed.rs`).

## Eviction

The runner's disk-pressure cleanup can evict the whole cache, but skips it while
a fill is running; it never deletes individual entries. See
[scheduling.md](scheduling.md#disk-admission).

Local bases also count against the job disk. Their fourth pressure pass runs
after whole workspaces and staging, evicts oldest last use first and skips
leased bases. The build-folder cap, stale-unit sweep and whole-workspace pass
never touch a base. Tests:
`fourth_pass_evicts_oldest_base_after_workspaces_and_staging_before_refusal`,
`cap_stale_sweep_and_whole_workspace_pass_leave_base_untouched`
(`crates/motor-runner/src/tests.rs`).
