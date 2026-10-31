# Compile cache and warm builds

Linux jobs share one compiler cache per runner instance, so a job does not
recompile dependencies that another job, or a background build, already
compiled. Jobs only read the cache. A separate trusted account fills it from the
repository's default-branch tip, never from a submitted branch. This document
covers who reads and writes the cache, what goes into a fill, and the rules that
keep a job from poisoning it. [architecture.md](architecture.md) shows where the
cache sits; [job-isolation.md](job-isolation.md) describes the reader's sandbox.

The Windows backend has its own per-workspace cache disks, described in
[windows-jobs.md](windows-jobs.md#cache-disks).

## Leased runners

Boat profiles never queue background cache fills or warm builds, and a tenant
VM queues none unless its provisioning holds a remote cache write key. An
idle-started fill can outlive the idle period, compete for leased CPU time, or
hold a setup lock when the next job arrives. Boat avoids that because it only
reads the cache. A tenant VM given a write key accepts the delay: it fills its
own cache with its own capacity, as the build server does, one fill at a time
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
Boat spills omit both, as before. Ordinary build-server channels retain fills.
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
Otherwise identical sources produce unrelated entries and every lookup misses.
Test: `reader_and_writer_have_identical_hashed_cargo_inputs`.

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
  reuses the previous fill. Tests:
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
- A failed descriptor is not retried until it changes or the runner restarts.
  Test: `failed_descriptor_is_suppressed_until_change_or_restart`.

### After a fill

The runner records the files the writer created (with `inotify`, without
walking existing entries) and repairs their permissions so readers can open
them. Overflowing the watch, or an unreadable entry, fails the fill rather than
advertising it. Tests: `cache_fill_repairs_only_changed_entries_and_never_follows_symlinks`,
`cache_fill_tracking_and_repair_fail_closed`. The writer's scratch target is
discarded; it is never published as a shared target.

### Without a cache account

If `motor-cache` is missing or invalid, fills and cache tracking are disabled;
foreground jobs still run. Test: `unavailable_writer_account_disables_fills_and_cache_tracking`.

## Eviction

The runner's disk-pressure cleanup can evict the whole cache, but skips it while
a fill is running; it never deletes individual entries. See
[scheduling.md](scheduling.md#disk-admission).
