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
| Each sibling, at its own absolute path (and next to the workspace under its directory name) | Read-only | The runner's sibling checkout |
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
