# Contract: job isolation

This contract states what a job can see and do on the runner: its filesystem,
its network, the account it runs as, and its limits. Linux jobs run in a
transient systemd service on the build server; Windows jobs run in a fresh
virtual machine, covered in [windows-jobs.md](windows-jobs.md). The threat model
built on these rules is in [SECURITY.md](../SECURITY.md).

The code is `crates/motor-runner/src/isolation.rs` (the sandbox and its
`systemd-run` command), `isolation/privilege.rs`, `systemd.rs`, `network.rs`
and `network_linux.rs`. Each rule names the test that proves it. Tests whose
names start with `isolated_` are operator-only: they start real sandboxes and
run in the [server gate](server-gate.md), not in hosted CI. The others run in
the portable suite.

## 1. When isolation applies

1.1. `motor-runner serve` on Linux always isolates jobs. A missing prerequisite
(the `motor-job` account, ACL tools, delegated `sudo` rules, namespace or mount
support) refuses startup or the job; a job never falls back to running as the
runner's own account. Source: `crates/motor-runner/src/main.rs`
(`config.isolate_jobs = cfg!(target_os = "linux")`). Tests:
`host_backend_refuses_isolated_launch_without_prepared_sandbox`
(`crates/motor-runner/src/backend_tests.rs`) for the launch path; the startup
refusal for each missing prerequisite is not covered by a test.

1.2. `Config::new` without that flag uses an in-process backend for tests. It
is not an isolation boundary.

## 2. Accounts

2.1. Every foreground job runs as `motor-job`, a dedicated non-root,
non-administrative account without a login shell. Test:
`isolated_host_backend_launch_runs_in_motor_job_service_with_022_umask`.

2.2. Compile-cache fills run as `motor-cache`, a separate account; see
[compile-cache.md](compile-cache.md). Each sandbox account must exist and must
differ from root, from the runner's account and from the other sandbox account,
and must not belong to `sudo`, `admin`, `adm`, `lxd` or `docker`. Test:
`writer_identity_refuses_missing_aliases_and_administrative_groups`
(`crates/motor-runner/src/isolation/writer.rs`). The runner itself runs as its
service account and never executes job code as that account.

2.3. A job has no passwordless `sudo`. Test: `isolated_job_has_no_passwordless_sudo`.

2.4. A job runs with `NoNewPrivileges=yes` and an empty capability bounding
set, so setuid binaries and file capabilities give it nothing. Test:
`isolated_job_runs_with_no_new_privileges`.

## 3. Filesystem

The sandbox has its own empty root directory (`RootDirectory=`) with only the
paths below bound in. `ProtectSystem=strict`, `ProtectHome=tmpfs`,
`PrivateTmp=yes`, `PrivateDevices=yes` and `ProtectProc=invisible` apply on top.

| Path inside the job | Access | Source |
| --- | --- | --- |
| The workspace, at the caller's absolute path | Read-write | The runner's checkout of the snapshot |
| The workspace's `.git` | Read-only | Same checkout |
| Each sibling, at its own absolute path (and next to the workspace under its directory name) | Read-only | Its immutable per-commit sibling snapshot; the host checkout only in legacy in-place mode |
| `/run/motor/target` and the per-workspace target path | Read-write | The workspace's private build output |
| `/motor-home` (`HOME`), with a private `CARGO_HOME` at `/motor-home/cargo` | Read-write | Per-job directory |
| `/motor-home/cargo/registry`, `/motor-home/cargo/git` | Read-only | The shared Cargo registry and Git sources |
| `/tmp` (`TMPDIR`, `TMP`, `TEMP` point at `/motor-home/tmp`) | Read-write | Per-job directory |
| `/dev/shm` | Read-write, private tmpfs with mode `1777`, `nosuid,nodev`, and a size cap equal to `MemoryMax` (executable, matching the host mount) | Per-job `TemporaryFileSystem=` mount |
| `/usr`, `/bin`, `/lib`, `/lib64`, the shared Cargo `bin`, the rustup home, Bun's `bin` if present | Read-only | Host toolchains |
| `/etc/passwd`, `/etc/group`, `/etc/nsswitch.conf`, `/etc/ld.so.cache`, `/etc/alternatives`, `/etc/ssl/certs`, `/etc/ssl/openssl.cnf` | Read-only | Host files |
| `/etc/hosts` | Read-only | Generated; see 3.5 |
| `/motor-cache` | Read-only, optional | The shared compile cache |
| `/motor-tools` | Read-only | Compiler wrapper and build-slot supervisor |
| `/motor-slots`, with its existing lock files writable | Read-only directory | Shared build-slot locks, when the build-slot supervisor is in use |
| `/etc/resolv.conf` | Read-only, outbound jobs only | Generated; see 6.3 |
| `/motor-output/{stdout,stderr,exit,cache-stats}` | Write to existing files only | The job's durable output |

Sibling snapshot paths may be in a different allowed workspace root. The
runner authorizes their resolved destinations just as it does the workspace;
neither nesting nor colliding worktree aliases are supported. The mount remains
at exactly the supplied canonical path, with a read-only worktree alias.
Operator test: `isolated_snapshot_sibling_in_another_allowed_root_is_read_only_at_its_path`.

Source: `Sandbox` construction in `crates/motor-runner/src/isolation.rs`.
Test for the command shape: `systemd_job_command_preserves_shell_source_and_forces_private_tmp`.
The complete mount list is not covered by one test; the
`systemd_job_command_enforces_recorded_cpu_set_and_memory` test asserts that
`/dev/shm` is sized to `MemoryMax`, and the rules below name tests for other
properties. A Rust job also gets `CARGO_TARGET_DIR=/run/motor/target` and
`SCCACHE_DIR=/motor-cache`.

Boat sandboxes and tenant VMs have no shared compile cache: their jobs get
no `/motor-cache` mount, no `RUSTC_WRAPPER` and no `SCCACHE_*` variables. A Rust
job there still builds in the same fixed folder as on the build server:
`CARGO_TARGET_DIR=/run/motor/target`, bound to the workspace's own `target/`, so
build paths and `./target` behave identically everywhere. Tests:
`hosted_jobs_bypass_shared_compile_cache_but_keep_the_fixed_target_folder`,
`tenant_rust_launch_builds_in_fixed_target_without_compile_cache`
(`crates/motor-runner/src/backend_tests.rs`).

3.1. A job cannot read the runner's state, mirrors, other workspaces or other
jobs' sandboxes. Tests: `isolated_job_cannot_access_runner_or_other_jobs` and
`isolated_jobs_receive_private_shm_that_disappears_after_they_end` (operator-only).

3.2. A job can read shared registry sources but cannot write them, and it
cannot write the shared compile cache. Tests:
`isolated_job_can_read_0640_shared_registry_crate_after_preparation`,
`isolated_job_cannot_write_shared_registry`,
`isolated_reader_hits_across_workspaces_and_cannot_poison_cache`
(`crates/motor-runner/src/isolation/writer.rs`).

3.3. A job's files are created with umask `022`, whatever the runner's umask.
Tests: `isolated_job_uses_022_umask_and_creates_755_directories_under_loose_parent`,
`job_process_sets_022_umask_independently_of_daemon` (`platform.rs`).

3.4. A job may change or delete its own workspace files, including in ways that
make its own cleanup fail. It cannot gain mounts or privileges. Changes are not
copied back to the caller. Not covered by a test beyond 2.4 and 3.1.
3.4.1. The workspace's `.git` is read-only because its objects are shared with
the runner's mirror of the repository. Git commands that only read work; any
command that writes the index, refs or objects fails, including `git add`,
`git checkout -- <path>`, `git stash` and `git commit`. A tool that needs a lock
or a scratch file puts it under `$TMPDIR` or the build folder instead. A script
that edits a tracked file and restores it afterwards can save the original
under `$TMPDIR` and copy it back. When it needs Git itself, a private index and
object store work too: set `GIT_INDEX_FILE` and `GIT_OBJECT_DIRECTORY` to paths
under `$TMPDIR`, and `GIT_ALTERNATE_OBJECT_DIRECTORIES` to the repository's
`.git/objects`. Not covered by a test.

3.5. `/etc/hosts` contains `localhost` and IPv6 loopback names, plus the names
a repository declares in a tracked `.motor/hosts` file. Declared names may map
only to loopback addresses; a public address refuses the job before its sandbox
is created. Tests: `isolated_job_resolves_localhost_to_loopback`,
`isolated_job_resolves_declared_loopback_hosts_including_child`,
`isolated_job_refuses_public_hosts_before_start`, and the parser tests in
`crates/motor-runner/src/isolation/hosts_tests.rs`.

3.6. Only the shared Cargo registry and Git sources, toolchains and system files
above are visible from the host. Package lifecycle scripts never run as the
runner: before the job starts, the runner installs JavaScript dependencies from
a checked-in lockfile without running their scripts, and fetches locked Cargo
sources. Tests: `isolated_nested_javascript_jobs_resolve_offline_and_reuse_trusted_cache`
(`crates/motor-runner/tests/it/javascript.rs`), and the tests in
`crates/motor-runner/src/isolation/javascript_tests.rs`.

3.7. A caller root never changes what a job sees at `/Users`. Caller paths under
a configured prefix keep their own spelling, and paths outside it are served
exactly as before; see 3.8.

## 3.8 Caller-root mapping

A Linux caller keeps its checkout under `/home/<user>/...`, but no runner may
store it there: the build server's own `/home` holds the operator account, its
toolchains and the runner binary, and Boat's provider owns `/home` inside the
sandbox. A *caller root* therefore pairs one caller prefix with a host
directory the runner owns, `PREFIX=HOST` for short, for example
`--caller-root /home=/srv/build/callers/main/home`. The runner stores the
checkout under the host directory and binds it, read-write, at the caller's
`/home` path. Every path in the job, its output, `pwd`, `$PWD`, git's
`safe.directory` entries, `WorkingDirectory` and its compiler diagnostics reads
`/home/...`; nothing names the host directory.

This section states the contract a caller root hands the job. The operator side
(directories, unit flags, rollout and rollback) is in
[runner-install.md](runner-install.md#9-optional-caller-roots-for-home-checkouts).
Source: `crates/motor-runner/src/caller_path.rs` and
`crates/motor-runner/src/caller_path/` (`startup.rs`, `owner.rs`, `trash.rs`,
`forget.rs`).

3.8.1. **One host directory per instance.** Main uses
`/srv/build/callers/main/home` and the `e2e` instance
`/srv/build/callers/e2e/home`; Boat uses
`/var/lib/motor-boat/state/callers/home`. No two instances share one, so the
instance's own `daemon.lock` fences every mutation under it. The directory is
never the caller's real `/home`, never inside the state directory, and never
inside an owned workspace root. Test:
`caller_root_spelling_and_option_refusals_name_both_paths`
(`crates/motor-runner/src/caller_path/startup_tests.rs`).

3.8.2. **Owner marker.** Every start, and every `forget-caller-root`, reads
`HOST/.motor-owner` by a no-follow walk from `/`; a symlinked component or
marker refuses before anything outside the directory is opened. The marker names
the instance and the state directory that own the tree. A start refuses, naming
the path and both values, when the marker names another instance or state
directory, is unreadable or malformed, or is absent while the directory holds
entries. An empty directory is claimed by writing the full marker to an unnamed
file and publishing it with `linkat`, so exactly one of two concurrent first
starts wins and a crash before the link leaves the directory empty. The first
start with any caller root also writes `<state-dir>/caller-roots-ever-enabled`;
nothing removes it. Tests: `marker_claim_is_durable_and_names_instance_and_state`,
`absent_nonempty_malformed_and_nonregular_markers_refuse_without_state`,
`symlink_marker_and_ancestor_never_open_outside_fifo`,
`concurrent_first_claims_have_exactly_one_winner`,
`marker_crashes_before_link_leave_no_entry_and_after_link_can_resume`
(`crates/motor-runner/src/caller_path/owner.rs`).

3.8.3. **Mappings are fixed once enabled.** At start the runner reads every
mapping named by the catalog files and by every job record. A prefix missing
from the current `--caller-root` set, or configured with a different host
directory, refuses the start, naming the prefix and both remedies: restore the
flag, or run `forget-caller-root`. Nothing is reclassified, parked or deleted
automatically, and a refused start changes no catalog file, record or tree. A
restart with the same mapping, including a Boat stop and resume, replays against
the recorded host directory. Tests:
`sole_dual_key_pin_refuses_before_cleanup_in_both_move_directions`,
`same_mapping_restart_preserves_catalog_and_retained_job_pins`
(`crates/motor-runner/src/tests/caller_persistence.rs`).

3.8.4. **Spelling and reserved names.** A mapped path must be spelled
canonically: absolute, no `.` or `..` component, no repeated separator and no
trailing slash, so each stored tree has exactly one accepted spelling. A caller
path equal to the prefix (`/home` itself) is refused for every field, as is any
path with a component directly beneath the prefix whose name begins with
`.motor-`: the host directory's trash (`HOST/.motor-trash-<instance>`) and its
owner marker.

3.8.5. **Nested workspaces are refused under `/home` exactly as under
`/Users`.** A workspace, repository or sibling that lies inside, or contains, a
catalogued workspace, a sibling, a live job, a running admission or a running
prepare is refused with the same `workspace_setup_failed` detail, in both
orders, whether a job or a setup holds the first one. Mapped trees have one
addition: a derived cache-fill checkout (the `.motor-base-<instance>-<name>`
checkout a warm build uses) also counts as a known workspace, so a fill never
nests a workspace and a workspace never nests a fill. The detail names both
caller paths and the refusal happens before anything is created. `/home/u/proj2`
is accepted beside `/home/u/proj`. Tests:
`mapped_nested_workspaces_fence_catalog_running_starting_admission_and_prepare_in_both_orders`,
`mapped_fill_checkouts_join_the_nested_known_set`,
`queue_base_skips_a_mapped_fill_that_nests_a_workspace_and_retries`
(`crates/motor-runner/src/tests/caller_authorization.rs`); the `/Users`
equivalents are `nested_workspace_is_refused_before_any_workspace_is_created`
(`crates/motor-runner/src/tests/nested_workspaces.rs`).

3.8.6. **Trash.** Each host directory keeps one runner-private trash,
`HOST/.motor-trash-<instance>`, created `0700` and checked runner-private. Every
deletion of a mapped tree (drop, expiry, pressure eviction, retirement,
`forget-caller-root`) renames into that trash first and removes the record
second; a renamed tree is purged at once, and leftovers are purged at the next
start. Nothing lands in the state directory's `trash`. Tests:
`mapped_drop_renames_into_h_trash_even_on_states_mount_then_purges`,
`mapped_expiry_and_pressure_use_host_paths_and_private_trash`,
`only_caller_root_flag_warms_drops_expires_and_pressure_evicts`,
`mapped_trash_requires_runner_private_mode_and_never_falls_back`
(`crates/motor-runner/src/tests/caller_trash.rs`).

3.8.7. **Returned files.** Returned files are read from the stored checkout and
reported, like every other caller-visible path, in caller spelling: a refusal
detail, a `workspace_changes` entry and a return manifest path name
`/home/...`, never the host directory. A link into the workspace is followed; a
link whose target leaves the caller workspace, including any target spelled with
the host directory, is `escape`. Tests:
`mapped_run_executes_in_the_host_checkout_and_returns_files_at_caller_paths`,
`mapped_chunked_returns_refuse_outside_and_host_form_links`,
`mapped_return_reads_use_host_files_and_refuse_escapes`,
`mapped_writer_refusal_details_name_the_caller_not_host_storage`
(`crates/motor-runner/src/tests/caller_authorization.rs`).

3.8.8. **`forget-caller-root`** is the only way to retire a mapping. It takes
the instance's full serve argument list and the prefix:

```sh
motor-runner forget-caller-root --prefix /home --instance main --state-root <state-root> \
  --caller-root /home=/srv/build/callers/main/home # ... the instance's other serve options
```

The list is parsed exactly as `serve` and `check-options` parse it, so the
instance, state root, Boat profile and owned roots match the serving process,
and an instance's configuration is copied from its unit rather than retyped. A
`--caller-root` entry for the forgotten prefix must name the recorded host
directory: with a different one, forget refuses and names both values. Entries
for other prefixes are parsed and ignored.

- **Locks.** It takes `<state-dir>/daemon.lock` without waiting, so it requires a
  stopped runner and fails while the lock is held. It reads the owner markers of
  the forgotten host directory and of every other host directory a pinning
  record names, by the same no-follow read.
- **Live-unit fence.** It refuses before any mutation while a live
  `motor-job-*` unit belongs to this instance's jobs: the unit each job record
  names, plus the unit named by every job ID under `<state-dir>/jobs`. Linux job
  unit names carry no instance, so units of other instances share the same
  `systemctl` listing but never block this instance's forget; the operator still
  drains another instance before forgetting its prefix. Test:
  `forget_refuses_while_a_live_job_unit_exists_with_the_same_list_units_query`,
  `forget_proceeds_with_only_unrelated_live_units`,
  `forget_fence_with_a_live_unit_changes_neither_tree_nor_record`
  (`crates/motor-runner/src/tests/caller_forget.rs`).
- **Steps.** Each tolerates work already done, so a kill at any point leaves a
  runner that still refuses to start and a rerun that converges: every child of
  the host directory except its trash and marker is renamed into the trash; every
  tree outside it owned by a pinning `caller-catalog.json` entry alone (a
  workspace, a warm-build fill checkout, a legacy checkout) goes through the
  normal deletion path into the trash of the host directory that holds it; the
  trashes are purged synchronously; and only then are the pinning
  `caller-catalog.json` entries and job records removed, after clearing
  `sibling_freshness` on the `catalog.json` record of a key found in both files.
  No `catalog.json` record is removed. The host directory, its empty trash and
  its marker remain, and the next start without the flag succeeds. Tests:
  `forget_sweeps_home_keeps_dual_keys_and_leaves_data_byte_identical`,
  `forget_with_flagged_serve_args_reaches_the_same_end_state`,
  `forget_with_mismatched_flagged_host_refuses_naming_both_and_changes_nothing`,
  `forget_rerun_converges_after_kills_in_steps_one_to_three`,
  `forget_rerun_converges_after_freshness_write_with_pins_remaining`,
  `forget_refuses_a_swapped_trash_and_leaves_the_sentinel`.

3.8.9. **Rollout.** Caller roots arrive in two steps: first a runner binary that
understands `--caller-root` but serves no caller root, then the units that carry
the flag. No binary that predates mapping may run against mapped state. The
operator procedure is in
[runner-install.md](runner-install.md#9-optional-caller-roots-for-home-checkouts).

## 4. Processes

4.1. A job has a private PID namespace and cannot see or signal processes
outside it. systemd >=257 uses `PrivatePIDs=yes`; the explicit Boat profile on
older systemd uses the runner's `job-pid-init` bootstrap, then execs the same
bash wrapper as PID 1 with the job beneath it. The bootstrap temporarily starts
as root, mounts namespace-local proc while retaining systemd's protected proc
submounts, and drops to `motor-job` with no capabilities, no-new-privileges,
a fresh user-owned session keyring and namespace seccomp before job code runs.
The trusted supervisor remains outside the namespace in the same cgroup. Tests:
`isolated_job_cannot_see_another_accounts_process`,
`boat_pid_init_unit_changes_only_what_the_init_needs`,
`namespace_filter_refuses_new_pid_namespaces_and_clone3_on_every_path`.
The live Boat probe checks PID 1, `/proc/1/cgroup`, outside-PID ESRCH, credentials,
capabilities, keyring ownership, proc protections and setsid-child cleanup;
portable tests do not establish those privileged transitions.

4.1.1. What a job sees in `/proc`: its own processes only
(`ProtectProc=invisible` hides other accounts'), plus the machine-wide entries
such as `/proc/stat`, `/proc/meminfo`, `/proc/cpuinfo`, `/proc/loadavg` and
`/proc/uptime`. `ps`, test harnesses and system-information libraries read
those (`ps` needs the boot time in `/proc/stat`). `ProtectKernelTunables=yes`
keeps `/proc/sys` and the other kernel-setting files read-only. Boat jobs see
exactly the same: the Boat profile does not set `ProcSubset=pid`, which would
hide every non-process entry and make `ps` fail. On Boat those entries describe
only the sandbox VM, which admits one job; on the build server they describe the
shared host, as they always have. Test:
`boat_command_preserves_isolation_and_limits_without_private_pids`
(`crates/motor-runner/src/isolation/tests.rs`); the live Boat probe checks the
mount and the entries from inside a job (`script/boat-image/README.md`).

4.2. The command starts only after the runner has written the job's durable
record: the service waits on a one-byte gate on its stdin, and end-of-file runs
nothing. After the gate the command's stdin is `/dev/null`. Tests:
`isolated_gate_eof_executes_nothing`, `isolated_job_stdin_is_eof_after_durable_gate`.

4.3. Cancellation and timeout kill the whole cgroup, including children that
ignore `SIGTERM`. Test: `isolated_cancel_kills_entire_cgroup_including_term_ignoring_child`.

4.4. A job starts with no ignored or blocked signals. Test:
`isolated_job_starts_with_no_ignored_or_blocked_signals`.

4.4.1. On Boat's older-systemd bootstrap path, the wrapper's exit file remains
authoritative and the supervisor mirrors its exit or terminating signal for
systemd's status fallback. Status 226 is a retry-safe refusal only with a
complete failure line in the root-only setup file, no wrapper exit code and
no job output. With neither a valid exit file nor that evidence, execution is
unknown; a wrapper-recorded exit 226 remains an ordinary job exit. Tests:
`supervisor_reproduces_real_exit_and_sigkill_wait_status`,
`boat_pid_namespace_setup_failure_is_refused_before_start_not_reported_as_exit`.

The exit inode is opened no-follow and parsed with a 33-byte read (32 bytes
maximum plus one overflow byte). That observation is carried into classification,
never reread, including for oversized or sparse files. Supervisor wait failure
uses status 227 and no retry-safe record; without a wrapper exit status the
outcome remains unknown. The supervisor never duplicates a child's core dump;
it mirrors the terminating signal with its own core-dumped bit clear.
The fallback also starts its own session, re-arms parent-death SIGKILL after
the credential drop, checks the supervisor pidfd to close the PPid-zero race,
and validates moved proc protections against a live independent reference.
Tests: `post_credential_drop_rearms_parent_death_signal`,
`job_group_sigcont_does_not_reach_supervisor_session`,
`supervisor_wait_failure_cannot_write_a_setup_record`,
`proc_protections_must_equal_the_independent_reference_after_moves`.

4.4.2. How a job can tell it is inside a motor job sandbox. On every Linux
host that runs jobs (the build server, a Boat sandbox and a hosted tenant VM)
these hold, and callers may rely on them:

- The job's process runs in the cgroup of its own transient system service,
  `/proc/self/cgroup` reads `0::/motor.slice/motor-jobs.slice/motor-job-<uuid>.service`, with a
  UUID in lowercase hyphenated form. It is the id of the runner that started the
  unit and need not equal the job id the caller sees: a hosted job runs under
  the tenant runner's own id. The name comes from one function, `host_unit_name`
  in `crates/motor-runner/src/backend.rs`.
- That cgroup is not delegated to the job: its directory and control files are
  owned by root, so the job cannot change its own limits. The one exception is
  `memory.pressure`, which systemd gives to the job account (mode 0644) so a
  process can watch its own memory pressure; it changes no limit. Both parent
  slices, `motor.slice` and `motor-jobs.slice`, are owned by root.
- PID 1 in the job's PID namespace (4.1) is the bash wrapper, running as the job
  account.
- The job account is `motor-job` (2.1). The tenant image recipe pins it to the
  build server's uid and gid (999 and 986). Boat lets the system choose a free
  system uid and a dedicated primary group when building each image, and records
  the actual identity in the image inventory. Callers should check the sandbox
  properties above, not a particular account number.

A caller that checks these should check the full shape (slice, prefix, UUID
and suffix), not a prefix, and refuse when any of them differs. The live Boat
probe checks PID 1 and `/proc/1/cgroup` (4.1); the cgroup path shape and
ownership are not covered by a test.

4.4.3. All job and cache-writer services, snapshot dependency helpers, Windows
preparation and QEMU services share `motor-jobs.slice`. Its `MemoryHigh=75%`
throttles and reclaims before `MemoryMax=85%` invokes cgroup OOM; percentages
refer to physical host RAM, so the same policy scales down on Boat and tenant
VMs. Swap is disabled for this slice. The remaining 15% is headroom for the
runner and system, not a reservation against unrelated host workloads. The
per-job limits remain unchanged. A slice cap bounds actual aggregate use,
unlike charging admission for each job's declared maximum, which would reject
low-memory jobs unnecessarily. CPU admission is unchanged.

Work services have `OOMScoreAdjust=900` and `OOMPolicy=kill`, so an OOM victim's
remaining processes are stopped. Runners stay outside that slice at `-900`.
Their `OOMPolicy=continue` keeps a killed launcher or permission-helper child
from making systemd stop the control-plane service as well.
The unprivileged build-server user manager cannot lower an inherited OOM score:
`script/runner/install-units.sh` therefore installs a privileged
`user@<operator-uid>.service` drop-in as well as the user runner units and the
system slice. Drain every instance and reboot before using the new negative
user-service score; reloading alone does not change the running user manager.
Boat and tenant runners are system services, so PID 1 can set their score
directly. The separate root permission-repair CLI raises its own score to 900;
root runners' in-process permission walks are still part of the runner.

Launch refuses a missing/unbounded slice rather than silently using systemd's
implicit unlimited slice. A killed launcher before gate release is setup
refusal, not an executed/lost job. A victim of an ancestor OOM still increments
its own `oom_kill` counter even if its own `max` and `oom` counters stay zero;
the existing OOM counts and per-job vCPU advice remain available. More vCPUs
raise only the per-job limit, not the shared host budget.

Tests: `installed_units_reserve_host_memory_and_protect_user_runners`,
`missing_or_unbounded_job_slice_refuses_execution`,
`dead_launcher_never_counts_as_a_started_job_even_if_unit_is_active`,
`ancestor_slice_oom_keeps_victim_counters_and_vcpu_advice`, and the operator-only
`isolated_job_memory_budget_and_oom_priority_protect_runner` (real cgroup limits,
job membership and `/proc` OOM scores; it does not exhaust host RAM).

4.5. The command runs as `bash -c <command>` with the request's working
directory. Its environment is the request's filtered variables (5.1) with the
runner's values forced for `HOME`, `CARGO_HOME`, `RUSTUP_HOME`, `PATH`,
`TMPDIR`, `TMP`, `TEMP`, `USER`, `LOGNAME`, `SHELL`, and `CARGO_NET_OFFLINE=true`.
Test: `systemd_job_command_preserves_shell_source_and_forces_private_tmp`
(the temporary-directory overrides); the other forced values are not covered by
a test.

## 5. Environment from the caller

5.1. Only build variables cross from the caller: names starting with `RUST_`,
`CARGO_` or `NEXTEST_`, plus `RUSTFLAGS`, `RUSTDOCFLAGS`, `CI`, `NO_COLOR`,
`CLICOLOR` and `CLICOLOR_FORCE`. `CARGO_HOME`, `CARGO_INSTALL_ROOT`,
`CARGO_TARGET_DIR` and `CARGO_BUILD_*` other than `CARGO_BUILD_JOBS` are
dropped, as is any name containing `TOKEN`, `SECRET`, `PASSWORD`, `KEY` or
`CREDENTIAL`. The client and the runner apply the same predicate
(`motor_protocol::allowed_env`). Tests:
`client_and_runner_share_one_environment_predicate`,
`transport_spy_filters_names_and_values_before_serialization`
(`crates/motor-module/tests/it/safety.rs`).

5.2. The `accepted` record lists the names that were dropped, never their
values. Test: `accepted_reports_dropped_env_names_without_env_values`
(`crates/motor-module/tests/it/recovery.rs`).

## 6. Network

6.1. By default a job has its own network namespace with only a loopback
interface (`PrivateNetwork=yes`). It can bind any port, including low ports, on
its own loopback, and reach nothing else. Tests:
`isolated_job_binds_low_ports_on_its_own_loopback_only`,
`isolated_net_offline_job_control`.
Linux job toolchain qualification also binds and connects `::1` in the job's
namespace, exchanges TCP data, and fails if IPv6 loopback is unavailable.
Its runtime report records the failing operation and errno, kernel command line,
IPv6 proc-file presence and namespace IPv6-disable settings. Tests:
`test_ipv6_loopback_probe_binds_and_connects`,
`test_every_linux_job_requires_ipv6_loopback`
(`script/runner/test_toolchain_parity.py`).

6.2. Outbound access is opt-in on both sides. The caller requests
`network: "outbound"`; `ck-motor` forwards the request only when its
`network_routing` setting is on; the runner grants it only when started with
`--job-network veth` and, at startup, finds its `motor_net` firewall table,
IPv4 forwarding and the `sudo` rules it needs
(see [runner-install.md](runner-install.md#6-optional-job-networking)). Any missing piece refuses the job
before upload with
`network_unsupported`; access is never silently downgraded or upgraded. Tests:
`network_admission_refuses_before_upload_with_missing_prerequisite`,
`network_hook_grants_only_opted_in_runs_and_reaches_terminal`
(`crates/motor-runner/src/tests/network.rs`),
`network_routing_off_refuses_before_contacting_runner`,
`network_routing_missing_grant_cancels_before_upload_and_refuses_once`
(`crates/motor-module/tests/it/recovery.rs`).

6.3. A job granted outbound access gets a veth pair into its namespace (its
side is `eth0`, IPv4 only) and reaches the internet through NAT. The runner
turns IPv6 off on `eth0`; on a kernel with no IPv6 at all (and hence no
`/proc/sys/net/ipv6`) there is nothing to turn off and the step is skipped.
Such a kernel cannot pass the job IPv6-loopback qualification. Any other failure of that
step, including a failed write while IPv6 exists, fails the attach. Test:
`eth0_ipv6_disable_is_skipped_only_when_the_namespace_has_no_ipv6`
(`crates/motor-runner/src/network_linux.rs`). The runner
checks at startup that the `motor_net` nftables table is present; the operator
installs it (see [runner-install.md](runner-install.md#6-optional-job-networking)). The table
(`script/runner/motor-net.nft`) lets the job open connections only to public
IPv4 addresses: private, loopback, link-local, carrier-grade NAT,
documentation, multicast and reserved ranges are rejected, as are connections
to the host and its gateway. Inbound connections to the job are dropped. Its
resolver is a fixed read-only `/etc/resolv.conf` naming public DNS servers.
Cargo stays offline by default (`CARGO_NET_OFFLINE=true`). Tests (operator-only,
need the table installed; they sample blocked destinations rather than every
range): `isolated_net_public_https_dns_read_only_resolver`,
`isolated_net_input_rejects_host_and_gateway`,
`isolated_net_forbidden_destinations_fail_fast_tcp_and_udp`,
`isolated_net_private_namespace_low_ports_and_cargo_defaults`,
`isolated_net_forward_drops_inbound_and_unrelated`.

6.4. Windows jobs never get outbound access; see [windows-jobs.md](windows-jobs.md).

## 7. Limits

7.1. A job's vCPU claim is its allowed CPU set (`AllowedCPUs=`), so it cannot
run on more CPUs than it reserved. See [scheduling.md](scheduling.md) for how
CPUs are chosen. Test: `systemd_job_command_enforces_recorded_cpu_set_and_memory`.

7.2. Memory is capped with `MemoryMax=`: 3.5 GiB per vCPU, with a 16 GiB floor.
The per-job `/dev/shm` tmpfs has an explicit size cap equal to `MemoryMax`;
tmpfs pages are charged to that job's memory cgroup. Tests:
`systemd_job_command_enforces_recorded_cpu_set_and_memory`,
`memory_max_scales_per_vcpu_with_a_sixteen_gib_floor`,
`memory_limit_gib_formats_floor_and_scaled_limits`.

7.3. The request's `timeout`, in seconds, runs from the moment the job starts,
not from acceptance. When it expires the runner kills the cgroup and the
terminal record carries `killed: deadline`. Test:
`engine::tests::cancellation_deadline_and_drop_kill_even_term_ignoring_groups`
(operator-only).

7.4. A job's disk use is not capped per job. The runner protects the host with a
free-space floor at admission; see [scheduling.md](scheduling.md#disk-admission).
A job can fill the disk while it runs. Not covered by a test.

7.5. There is no per-job limit on the number of processes or threads beyond
the memory cap and the host's own limits. Not covered by a test.

## 8. What isolation does not provide

- It is a filesystem, credential and network boundary inside one shared Linux
  kernel, not a virtual machine. A kernel vulnerability reachable from an
  unprivileged process crosses it.
- Jobs from the same caller share the `motor-job` account. Separate sandboxes,
  a private PID namespace and per-job directories keep them apart, but they are
  not separate users.
- A job granted outbound access can send anything it can read to the internet,
  including its workspace.
