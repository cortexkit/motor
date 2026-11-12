# Compile cache and warm builds

Linux jobs share one compiler cache per runner instance, so a job does not
recompile dependencies that another job, or a background build, already
compiled. Jobs only read the cache, with one exception: an account can let its
jobs on registered hosts add new entries to that account's remote compile
cache, as described in [Job writes to the remote cache](#job-writes-to-the-remote-cache).
Boat jobs and ccache stay read-only in every case. A separate trusted account
fills the cache from the repository's default-branch tip, never from a
submitted branch. This document covers who reads and writes the cache, what
goes into a fill, and the local build bases that can seed private build folders.
The repository's default branch and its locked dependencies are trusted; code
on other branches is untrusted. A malicious default branch can poison both the
shared compile cache and its build bases. A default-branch fill's output may
seed later jobs of the same account and repository.
A repository-configured rustc wrapper disables seeding for that job.
[ARCHITECTURE.md](../ARCHITECTURE.md) shows where the cache sits;
[job-isolation.md](job-isolation.md) describes the reader's sandbox.

The Windows backend has its own per-workspace cache disks, described in
[windows-jobs.md](windows-jobs.md#cache-disks).

## Leased runners

Boat profiles never queue background cache fills or warm builds, and a tenant
VM queues none unless its provisioning holds a remote cache write key. An
idle-started fill can outlive the idle period, compete for leased CPU time, or
hold a setup lock when the next job arrives. Boat avoids that because it only
reads the cache; Boat jobs also stay read-only when their account allows job
writes. A tenant VM given a write key accepts the delay: it fills its
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
`rustc` through sccache. The job cannot write entries in the runner's local
cache, including through build scripts or procedural macros, because that cache
belongs to another account and the mount is read-only. The only way a job adds
entries is the remote cache's create-only socket, which a job gets only when
its account allows job writes; see
[Job writes to the remote cache](#job-writes-to-the-remote-cache).

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

Fills therefore inherit the same nested-Cargo lock trap as jobs: a build script
starting another Cargo with its own `--target-dir` must remove
`CARGO_BUILD_BUILD_DIR` from that child command's environment. See
[job isolation](job-isolation.md) for the build-folder lock and workaround.

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

## Job writes to the remote cache

On a registered host, whether a tenant VM or a VM per job, the runner reaches
the account's remote compile cache, an S3-compatible object store, through a
gateway that holds the account's keys. A job never sees a key: it gets a Unix
socket and dummy credentials. By default every job's socket is read-only and
only trusted fills write. An account can also let its jobs save their own
compiler results, so a cache miss in one job becomes a hit for the next.

### Setting and eligibility

The broker keeps a per-account job write setting, off by default. It is never
supplied by the client. When the setting is on, the broker adds the optional
signed `cache_write` claim to the job's run token; an absent or false claim
means read-only.

A job writes only when all of these hold:

- its execution's `cache_write` claim was true when the execution was first
  admitted;
- it runs on a registered host, not on Boat;
- the account's provisioned remote-cache keys include a write key;
- the [emergency write block](#emergency-write-block) is not set.

The decision is taken once, when the execution is first admitted, and never
from a later reconnect or attach token or from the client's Run frame. A later
token carrying the claim does not upgrade a read-only job, and a later token
without it does not downgrade a write-enabled one: to stop a running job's
writes, stop the job or set the block. A missing write key leaves the job
read-only; the gateway never borrows another account's key.

Only sccache writes. ccache keeps its read-only socket, Boat jobs never receive
the claim and refuse a token that carries it, and a runner too old to know the
claim ignores it and stays read-only. Writes are sccache's own uploads of
finished compiler outputs; nothing copies a build folder into the cache after
a job.

Every job of a write-enabled account can store any bytes under any sccache key
in that account's part of the store, including a key another repository of
the same account will ask for later. Create-only writes stop a job from
replacing an existing entry, not from planting a bad new one. An account that
needs a cache only trusted builds can fill leaves the setting off.

Tests: `eligibility_requires_host_permission_isolation_tenant_role_and_account_key`,
`first_job_record_fixes_access_despite_later_run_or_attach_tokens`,
`keyless_blocked_or_wrong_account_admission_cannot_be_upgraded_later`
(`crates/motor-runner/src/tests/tenant_cache_write.rs`),
`first_claim_ceiling_requires_explicit_permission_registered_host_and_no_block`,
`later_run_attach_or_block_change_never_replaces_the_first_claim_ceiling`
(`crates/motor-host/src/state/cache_write_tests.rs`), and the operator-only
`isolated_tenant_cache_writes_use_admitted_claims` and
`isolated_tenant_cache_denials_survive_later_credentials_and_attach`
(`crates/motor-runner/src/tests/tenant_cache_write_isolated.rs`).

### Create-only writes

A write-enabled job's sccache socket is a create-only socket instead of the
read socket, never in addition to it. Reads on it behave exactly as on a
read-only socket: they use the read key and the same read limits, and any
failure is a miss. The job's environment differs from a read-only job's only
in `SCCACHE_S3_RW_MODE=READ_WRITE`. That variable is not what grants writing;
the socket is.

Every upload a job sends is signed with the write key, and the gateway adds
`If-None-Match: *` to it, so the store creates the object only if no object of
that name exists. Conditional headers sent by the job are dropped, never
forwarded. Fills keep unconditional uploads, because they repair entries. The
socket refuses listing, deleting, and any other tool's or account's part of the
store, as every cache socket does.

The store's answer to a job upload decides what the job sees:

- Success: the object was created, and the answer is passed on.
- 412, the entry already exists: the job gets 200 with an empty body, and the
  upload counts as a duplicate. When the store answers before the job has sent
  its whole body, the gateway reads and discards the rest within the time
  limit, or else closes the connection; it never reuses a connection with
  unread bytes.
- 409 or any other failure: the job gets 503, and the upload counts as failed.
  The gateway never resends a body; a retry by sccache is a new upload and gets
  the condition again.

No job upload ever reaches the store without the condition, and there is no
fallback to overwriting. A store that cannot honour the condition must keep
job writes off; the [create-only probe](#create-only-probe) checks this.
Cancelling a job during an upload leaves no partial object, while entries from
the job's earlier compiles remain.

Tests (`crates/motor-runner/src/remote_cache/gateway/`):
`puts_sign_the_gateways_create_condition_and_ignore_the_jobs_conditions`,
`job_create_refuses_delete_list_and_prefix_escapes_without_contacting_the_store`,
`reads_share_job_and_gateway_caps_with_read_only_sockets`
(`job_create_tests.rs`), `fake_store_conditionally_creates_without_overwriting`,
`full_body_duplicate_is_empty_and_preserves_bytes_and_client_reuse`,
`early_answers_close_instead_of_reusing_unread_body_bytes`,
`non_success_answers_fail_once_without_replaying_the_body`
(`store_answers_tests.rs`).

### Per-job write budgets

The runner limits each write-enabled job's uploads. Each limit is a runner
`serve` option:

| Option | Default | Limits |
| --- | --- | --- |
| `--remote-cache-max-job-put-objects` | 5000 | Objects a job may upload |
| `--remote-cache-max-job-put-bytes` | 8589934592 (8 GiB) | Bytes a job may upload |
| `--remote-cache-max-job-puts` | 16 | Uploads a job may have in flight at once |

The defaults never refuse an ordinary build but stop a runaway job. Tenant
provisioning does not set these options, so tenant runners use the defaults.

Before any byte goes to the store, an upload reserves one object and its
stated length. An upload that would pass either total, or the in-flight limit,
gets 503 without contacting the store and counts as refused. Duplicates and
failures keep their reservation, so repeated duplicates stay bounded; an
in-flight slot frees when its upload ends. The budget belongs to the job id
and holds across all of the job's sockets, connections and key rotations.
sccache's `.sccache_check` test object does not use up the object or byte
budget, but is still create-only and still subject to the block. Fills have no
job budget. After a refusal the job keeps reading the cache and compiles the
entry itself, uncached. These are per-job limits; there is no per-account write
budget.

Tests (`crates/motor-runner/src/remote_cache/gateway/write_budget_tests.rs`):
`runner_settings_supply_gateway_write_limits`,
`object_budget_survives_connections_socket_replacement_and_key_rotation`,
`byte_budget_checks_content_length_and_rejected_puts_reserve_nothing`,
`duplicates_and_failures_keep_object_and_byte_reservations`,
`simultaneous_puts_never_overshoot_object_or_byte_budgets`,
`concurrent_put_cap_spans_sockets_and_rotation_and_releases_on_completion`,
`sccache_check_is_create_only_but_exempt_from_object_and_byte_budgets`,
`fill_puts_ignore_all_job_write_limits`.

### Emergency write block

The block is a host-wide switch that only takes writing away; it can never
grant it. The operator sets and clears it through `motor-host`, which records
it so that it survives a restart; see
[the operator steps](hosted-host.md#job-writes-to-the-remote-cache). While it is
set, new admissions are read-only, and every running runner, tenant or job VM,
is told to refuse job uploads. From the moment a runner acknowledges, its job
uploads get 503 without contacting the store and count as refused. An upload
still sending its body is aborted before the last byte, so no partial object
becomes visible; an upload already fully sent completes. Reads and fills carry
on. Clearing the block restores writes for jobs that were admitted with them.

Tests: `block_between_keepalive_puts_denies_checks_and_clearing_restores_writes`,
`acknowledgement_closes_partial_upload_before_last_byte_and_never_publishes`,
`fully_sent_put_waiting_for_store_response_completes_while_blocked`
(`crates/motor-runner/src/remote_cache/gateway/write_block_tests.rs`).

### Create-only probe

The create-only probe checks that the account's store really honours
`If-None-Match: *`. The runner runs it through its own gateway with the
account's provisioned keys, using fresh object names in the account's sccache
area that sccache never asks for. The gateway cannot delete, so probe objects
expire under the bucket's lifecycle rule. It passes only if all four cases
pass:

| Case | Passes when |
| --- | --- |
| `concurrent_creates` | Simultaneous conditional creates of one name give exactly one success, the others 412 or 409, and the stored bytes are the winner's. |
| `existing_object` | A conditional upload of different bytes to an existing name gets 412 and leaves the bytes unchanged. |
| `interrupted_body` | An upload cut off mid-body leaves no object, and a later create succeeds. |
| `fill_race` | A job create racing an unconditional fill upload leaves exactly one of the two bodies, never a mix. |

The probe reports each case and returns a non-zero result when any case fails.
It is separate from `remote_cache_probe`, which only reports that a tenant
supports provisioning and is unchanged.

A passing probe against the account's live store is the operator's
precondition before job writes are enabled for that account. It is a rollout
step, not a CI check: nothing records the result or checks it automatically.
If the probe fails, writes stay off; there is no fallback to overwriting.

Tests: `all_four_store_criteria_pass_through_the_gateway_with_provisioned_keys`,
`a_store_ignoring_conditions_has_a_nonzero_probe_result`,
`every_criterion_detects_its_store_fault_without_hiding_other_case_reports`
(`crates/motor-runner/src/remote_cache/gateway/probe_tests.rs`).

### Counters

The gateway counts each job's accepted, duplicate, failed and refused uploads,
and the bytes of accepted uploads. At job end they join the job's remote cache
totals as `puts_accepted`, `puts_duplicate`, `puts_failed`, `puts_refused` and
`put_bytes_accepted`, and `motor-host` logs them by job id with the existing
hit and miss counts. Fields that are zero are omitted, so a read-only job's
totals look exactly as before. Like the hit counts, these are best-effort
figures reported by the runner, not usage or billing data. Test:
`job_totals_include_refusals_and_survive_new_bindings_and_key_rotation`
(`crates/motor-runner/src/remote_cache/gateway/store_answers_tests.rs`).

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

Background compile-cache fills have a 24 GiB memory floor, because some
repositories' builds need more than 16 GiB; in per-job mode, a fill VM's stated
memory is at least 24 GiB.

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

Linux writer units, including compile-cache fills and local base builds, have a
90-minute runtime cap. The runner-local `--writer-runtime-limit-secs` setting
changes that cap (default `5400`, must be positive). On expiry systemd stops the
whole unit, escalating to SIGKILL after two seconds; the failed fill cannot
publish a successful fill record or replace the current build base. The cap
covers execution, including preflight and warm-up steps, not queue wait or
trusted source preparation. The runner logs writer start, end and kill requests
with repository, unit and age so operators can identify a stuck writer.

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

On per-job hosts, fills receive an exported default-branch tip and the Git
dependency mirrors the fill needs through verified bundles on a fresh Fill
disk. They never mount a job-written filesystem. Current per-job settings have
no root-owned fixed-path repository allowlist, so per-job fills write the shared
compiler cache only; their disks are deleted, not promoted to local build bases
that seed later jobs.
Promotion requires fixed paths, which per-job hosts do not have yet.
See [per-job fill sources](hosted-host.md#per-job-fill-sources) for export limits,
storage accounting and VM lifetime.

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
or seeding. Build-directory preparation treats Cargo's dependency-information
(`.d`) files as belonging to this checkout when they name the fixed checkout
or sibling aliases, and only when the fixed plan exists; otherwise it uses the
legacy caller roots. Tests:
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
a base on another filesystem (`EXDEV`), a filesystem without reflinks
(`EOPNOTSUPP`), a partial clone or time-step failure) settles the intent,
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
