# Contract: Windows jobs

A caller can ask for a job to run on Windows. A runner instance started with
`--backend windows-vm` runs each such job in a fresh, disposable Windows virtual
machine on the Linux build server. This contract states how a Windows job is
requested, what it sees, how its paths and exit status differ from a Linux job,
and what is refused. [windows-image.md](windows-image.md) covers building and
qualifying the VM image; [wire-protocol.md](wire-protocol.md) defines the
records named here.

Tests in `crates/motor-runner/src/tests/windows_backend.rs` and
`windows_instance.rs` drive the production host code against a fake systemd
and a fake guest agent; they run in the portable suite on Linux. Tests named
`windows_qualification_*` (`crates/motor-runner/src/tests/windows_operator.rs`)
boot the real image and are operator-only.

## 1. Requesting Windows

1.1. A request selects Windows with `platform: "windows"`. An absent platform
means Linux. Test: `run_platform_uses_shared_tags_and_preserves_unknown_values`
(`crates/motor-protocol/tests/it/platform_contract.rs`).

1.2. `ck-motor` routes a Windows request to the instance named by its
`windows_runner_instance` and `windows_runner_state_root` settings. Without
them, or for an unknown platform, it refuses with `platform_unsupported` before
opening any connection. Tests: `unconfigured_windows_and_unknown_platforms_refuse_before_any_channel`,
`windows_config_load_requires_safe_distinct_paired_keys`
(`crates/motor-module/tests/it/routing.rs`).

1.3. A runner lists the platforms it serves in its `hello` (`platforms:
["windows"]` for a Windows instance; empty for Linux). `ck-motor` sends a Windows
run only to a runner that lists `windows`; otherwise it refuses with
`platform_unsupported` and sends no `run`. Tests:
`windows_hello_gate_refuses_old_peer_without_sending_run`,
`windows_hello_advertises_explicit_platforms`.

1.4. Each instance serves exactly one platform. A runner refuses a request for
any other platform, including an unknown one, with `platform_unsupported` and
`ran: none`, before every other admission check, and records the refusal
durably. The refusal detail names the requested and the served platform. Tests:
`platform_refusal_precedes_ordinary_admission_checks`,
`platform_refusals_are_durable_no_start_terminals`,
`platform_wire_refusal_names_requested_and_served_platforms`
(`crates/motor-runner/src/tests/platform.rs`).

## 2. Platform echo

2.1. A Windows instance echoes `platform: "windows"` in `accepted`. A Linux
instance's `accepted` omits the field, so older callers see unchanged bytes.
Tests: `platform_wire_echo_and_linux_hello_preserve_the_served_platform`,
`accepted_platform_echo_is_optional_and_round_trips`,
`linux_run_omits_platform_and_forwards_linux_echo`.

2.2. If a Windows request is accepted without the Windows echo, `ck-motor`
forwards the acceptance, cancels the job, and follows it to its terminal
record; it never lets the job run as if it were on Windows. Test:
`windows_wrong_or_absent_accepted_echo_is_forwarded_and_cancelled`.

2.3. The instance a job ran on is stored with the job, so attach, cancel and
status after a `ck-motor` restart reach the same instance. Tests:
`windows_run_binding_echo_and_reconnect_survive_module_restart`,
`detached_windows_cancel_retry_keeps_the_persisted_instance`.

## 3. What a Windows job sees

3.1. Every job boots its own VM from a copy-on-write overlay of the sealed
baseline image. The overlay is deleted after the job; nothing a job writes to
the system disk reaches a later job. Tests (operator-only):
`windows_qualification_no_persistence_offline_overlay_canaries`.

3.2. The VM has no network device. The command can use loopback inside the
guest and nothing else. A request for outbound network is refused with
`network_unsupported`. Tests: `windows_qualification_no_network_but_loopback_works`
(operator-only); source `crates/motor-runner/src/network.rs` (`unsupported`).

3.3. The command runs as the guest's standard, non-administrator `motor-job`
user, through Git Bash (`bash -c <command>`), inside a Windows Job Object that
is killed as a whole on cancel. The guest agent refuses an elevated token. Test:
`windows_qualification_cancel_kills_detached_grandchild` (operator-only).

3.4. The environment starts with Windows' `CreateEnvironmentBlock` for the job
user, after loading that user's profile, without inheriting the agent service's
environment. Failure to obtain the block refuses the job before launch. The
request's filtered variables are applied over this base (the same predicate as
Linux; see [job-isolation.md](job-isolation.md#5-environment-from-the-caller)),
with the agent's values forced, case-insensitively, for `PATH`, `RUSTUP_HOME`,
`HOME`, `USERPROFILE`, `CARGO_HOME`, `TMP`, `TEMP`, `SystemRoot`, `SystemDrive`,
`CK_REMOTE_SOURCE_COMMIT`, and `CARGO_NET_OFFLINE=true`. Standard Windows values
such as `PATHEXT`, `APPDATA` and `LOCALAPPDATA` survive. The agent does not
synthesize `HOMEDRIVE` or `HOMEPATH` if Windows omits them. Tests:
`standard_user_environment_survives_job_overrides`,
`request_overrides_base_but_not_agent_owned_values`,
`environment_source_failure_propagates_and_owned_blocks_are_always_released`,
`forced_environment_wins_case_insensitively_and_thread_counts_survive`
(`crates/motor-winagent`), and `windows_qualification_standard_user_environment`
(operator-only).

The initial native `PATH` puts Rust's `cargo\bin`, Git's `cmd`, the installed
Git `ucrt64\bin` (or `mingw64\bin`), Git's `usr\bin`, and PowerShell 7 before
Windows' machine-then-user `PATH`. Equivalent case-insensitive path entries are
deduplicated, keeping the first; managed tools win executable-name conflicts.
So a bare name that both Git and Windows provide resolves to Git's GNU tool,
even from `cmd /c`: `timeout`, `find`, `sort` and similar take GNU arguments.
Call the Windows tool by its full path when that is what you need, for example
`%SystemRoot%\System32\timeout.exe`.
Git Bash may prepend its own normal shell directories. A caller cannot replace
`PATH` through the request allowlist. Test:
`managed_paths_win_and_windows_machine_then_user_paths_survive`.

3.5. The toolchain is the baseline's machine-wide Rust for
`x86_64-pc-windows-msvc`, with MSVC Build Tools, Git, Bun and PowerShell
7.6.7 at `C:\Program Files\PowerShell\7`. The job can read and execute the
PowerShell installation but cannot write files or create entries anywhere in it.
Its directory is provisioned on machine `PATH` and precedes Windows PowerShell
5.1 in the job `PATH`; 5.1 remains available. Native children receive Windows
`TEMP`/`TMP` paths even when Git Bash displays a POSIX form. Tests:
`job_path_has_powershell_7_before_windows_powershell`,
`windows_qualification_standard_user_environment`, and
`windows_qualification_baseline_machine_wide_toolchain_and_msvc_build`
(operator-only).

3.6. The VM process on the host runs as the `motor-qemu` account in a transient
systemd unit with a private network, strict system protection, access only to
`/dev/kvm`, the instance's fixed CPU set, a memory cap of the guest's memory
plus 2 GiB, 256 tasks, and a size cap on the overlay's growth (default 40 GiB).
Tests: `windows_backend_qemu_unit_has_confined_account_fixed_paths_limits_and_offline_devices`,
`windows_qualification_qemu_and_preparation_accounts_are_confined` (operator-only).

3.7. One VM runs at a time: the instance fixes every job's weight and its
`max_weight` at 4, and the VM takes a host-wide slot lock. If the slot is held
(for example by the standing build VM), the job is refused before start with reason `runner_full` and `windows_slot_busy` in `refusal_detail`. Tests: `windows_weights_are_fixed_and_warm_builds_and_caller_catalog_are_disabled`,
`windows_backend_slot_busy_comes_from_child_code_75_before_hello_deadline`,
`windows_qualification_one_vm_slot_lock_refuses_without_boot` (operator-only).

## 4. Paths

4.1. Windows has no caller path to reproduce, so the runner maps it. A caller
path `P` becomes `C:\m` followed by `P` with `/` turned into `\`: `/Users/me/project`
becomes `C:\m\Users\me\project`. The workspace, siblings and `cwd` are
mapped the same way. Test: `every_invalid_windows_path_rule_is_refused`
(`crates/motor-winagent/src/lib.rs`).

4.2. The `started` record carries `path_mapping: {host_prefix: "/",
guest_prefix: "C:\\m"}` so the caller can translate paths in output back.
`ck-motor` forwards it unchanged. Linux jobs omit the field. Tests:
`windows_backend_streams_collects_and_preserves_full_width_status_at_wire_seam`,
`started_path_mapping_round_trips_native_windows_prefixes`,
`started_mapping_is_optional_and_absence_keeps_legacy_wire_bytes`,
`windows_mapping_and_full_width_outcome_forward_unchanged`
(`crates/motor-module/src/started_tests.rs`).

4.3. A path that Windows cannot hold is refused before start: empty, `.` or
`..` components, components ending in `.` or space, control characters or any
of `<>:"/\|?*`, and reserved device names (`CON`, `PRN`, `AUX`, `NUL`,
`COM1`–`COM9`, `LPT1`–`LPT9` and similar), with or without an extension. Tests:
`every_invalid_windows_path_rule_is_refused`,
`windows_backend_every_invalid_path_refuses_before_start_and_a_symlink_never_follows`.

4.4. Two paths that differ only in letter case are refused before extraction.
Test: `windows_backend_case_collisions_are_detected_before_case_insensitive_extraction`.

4.5. A tracked symlink is staged as a regular file whose content is the link's
target. Test: `windows_backend_stages_a_tracked_symlink_as_a_file_holding_its_target`.

4.6. The standard `motor-job` account holds `SeCreateSymbolicLinkPrivilege`
through a SID-specific local-security-policy assignment, not Developer Mode or
administrator membership. Jobs can create file and directory symlinks, including
absolute and UNC targets. Junctions and directory mount points already required
no such privilege. After the Job Object empties, collection runs impersonated as
the job user: it hashes link text and emits zero-payload symlink tar records,
never target bytes. Reparse directories are not traversed, a reparse source root
is refused, and unsupported reparse types fail rather than being followed.
Case-insensitive `target` subtrees remain excluded. The Linux runner saves this
archive as inert bytes plus a validated source-path manifest; it never unpacks
returned links onto the host. Staging still forbids link archive entries. Tests:
`collection_returns_file_directory_and_dangling_links_without_target_bytes`,
`collected_link_text_preserves_windows_absolute_unc_and_long_targets`,
`collected_links_are_only_inert_tar_bytes_on_the_host`, and
`windows_qualification_symlinks_do_not_escape_collection_or_cache` (operator-only).

## 5. What a Windows job may build

5.1. A trusted preparation step, run on the host as the `motor-prep` account,
fetches the repository's locked Cargo dependencies for
`x86_64-pc-windows-msvc` before the VM boots. The job then builds offline.
Test: `windows_qualification_qemu_and_preparation_accounts_are_confined`
(operator-only).

5.2. The workspace must have a `Cargo.lock`. Path dependencies outside the
transferred checkouts, Git dependencies over SSH, and registries other than
crates.io are refused before start. A repository's own `.cargo/config.toml` is
accepted. Test: `windows_backend_rejects_unsupported_manifests_but_accepts_cargo_config`.

5.3. Only the snapshot (`m/`) and the selected parts of the Cargo home
(`registry/index`, `registry/cache`, `git/db`, `git/checkouts`) enter the VM,
as a tar stream over the agent channel. Test:
`windows_backend_tar_contains_only_m_and_selected_cargo_home_members`.

5.4. `prepare` (workspace preparation without a command) is not supported on a
Windows instance and is refused before any work. Background warm builds do not
run there. Tests: `windows_prepare_is_refused_before_validation_reservation_or_filesystem_work`,
`windows_weights_are_fixed_and_warm_builds_and_caller_catalog_are_disabled`.

## 6. Exit status

6.1. A Windows job that exits ends with outcome `windows_exit` and the full
unsigned 32-bit value from `GetExitCodeProcess`, not a Linux-style exit code
or signal. Crashes carry their NTSTATUS (for example `0xC0000005`); a code with
both top bits set is an NTSTATUS failure. Tests:
`windows_backend_streams_collects_and_preserves_full_width_status_at_wire_seam`
(checks 0, 256, `0x7FFFFFFF`, `0xFFFFFFFF`, `0xC0000005`),
`roundtrip_and_full_width_exit_status` (`crates/motor-winagent/src/lib.rs`),
`windows_qualification_full_width_and_ntstatus_crash_round_trip` (operator-only).

6.2. Older callers that do not know `windows_exit` decode it as an unknown
outcome and must treat it as `outcome_unknown`, never as proof that nothing ran.
Source: `Outcome::WindowsExit` in `cortexkit-exec-remote-types`.

6.3. A cancelled command is terminated with `0xC000013A`; the terminal record
says `killed: cancel` or `killed: deadline`. Not covered by a portable test;
`windows_qualification_cancel_kills_detached_grandchild` (operator-only)
exercises cancel.

6.4. Once the runner has received the exit code, a later failure (collecting
changes, dismounting the cache, stopping the VM) does not change it. If the VM
fails before reporting an exit code, the outcome is `outcome_unknown`. Tests:
`windows_backend_missing_exit_is_unknown_but_post_exit_failures_preserve_status`,
`windows_backend_missing_dismount_reply_preserves_command_exit_and_discards_cache`.

## 7. Start, cancel and restart

7.1. The runner writes the `started` record to disk before it releases the
command in the guest. If that write fails, or the job is cancelled while the VM
boots, the command never runs. Tests:
`windows_backend_started_write_failure_destroys_unreleased_vm_and_dispatches_next`,
`windows_backend_cancel_during_boot_never_releases_command`,
`windows_backend_withdrawal_during_boot_destroys_prepared_vm`.

7.2. A command may run silently for as long as its timeout allows; there is no
agent inactivity deadline while it runs. Boot, transfer, collection and
shutdown each have bounds. Cancel never waits for the guest to answer. Tests:
`windows_backend_silent_commands_have_no_agent_deadline_and_cancel_never_waits_for_agent`,
`windows_backend_pre_start_bounds_and_untrusted_frames_refuse_and_clean_up`.

7.3. A started Windows job cannot be reattached after a runner restart. The
restarted runner stops the VM and ends the job as `outcome_unknown`; a deploy
therefore waits for started Windows jobs to finish. Tests:
`windows_reconcile_stops_orphan_units_and_uses_windows_names_for_status`,
`windows_started_unit_backed_job_keeps_drain_waiting`,
`windows_qualification_runner_sigkill_reconciles_boot_and_started_jobs` (operator-only).
See [drain-and-rollback.md](drain-and-rollback.md).

## Cache disks

Each workspace has its own 40 GiB build-cache disk, attached to the VM as a
second drive with the serial `motor-cache`; the guest mounts it at `C:\t\`
and junctions the workspace's `target` directory to that same volume root.
Unless the caller sets `CARGO_TARGET_DIR` (case-insensitively), the agent defaults
it to `C:\t`, keeping MSVC linker output paths short even for a long checkout.
Default builds still persist on that workspace's cache disk, and reading
`target/debug/...` in the checkout still works through the junction. An explicit
caller target directory may bypass this cache, just as before. The new environment
default overrides Cargo's `build.target-dir` configuration; a caller environment
value or Cargo's `--target-dir` selects a different path. Tests:
`job_target_dir_defaults_to_short_cache_mount`,
`caller_target_dir_wins_case_insensitively`, and
`windows_qualification_baseline_machine_wide_toolchain_and_msvc_build`
(operator-only for the last test).

A disk is keyed by the workspace, the baseline name
and the toolchain version, so a new baseline or toolchain starts cold.

A disk is kept for the next job only when the command reached a known outcome
(any exit code counts), the guest dismounted the disk cleanly, and the VM unit
exited normally. Otherwise the disk is discarded. Disks are evicted least
recently used under disk pressure, never while attached or recently used.
Tests: `windows_cache_key_includes_baseline_name_and_toolchain`,
`windows_cache_reuse_requires_clean_dismount_normal_exit_and_known_outcome`,
`windows_reconcile_discards_marked_disks_but_keeps_unattached_disks`,
`windows_pressure_is_lru_and_never_evicts_marked_recent_or_owned_disks`,
`windows_qualification_cache_isolation_and_hard_cancel_discards_disk` (operator-only).

Two workspaces never share a disk, but jobs in the same workspace do: a job can
leave files in the cache that the next job in that workspace reads.

The agent grants the empty physical `C:\t` folder before mounting the cache.
Only a newly formatted volume may receive the inheritable job-SID Modify ACL;
SYSTEM never repairs ACLs recursively on a reused, job-populated tree. If its
exact grant is missing, the agent reports `cache_grant_missing` without changing
the tree. The host stops the failed VM, discards the cache, logs the workspace's
cache-key hash, and retries preparation once on a fresh disk before opening the
execution gate. A second failure refuses the job, not a third boot. Dismount uses
the volume GUID, never the mutable workspace junction or `C:\t` pathname; the
mount folder is left in the disposable OS overlay. Tests:
`reused_cache_missing_grant_never_updates_acl`,
`mount_folder_refuses_links_and_nonempty_directories`,
`privileged_dismount_never_resolves_the_job_mount_folder`,
`windows_backend_missing_cache_grant_discards_and_retries_once_before_start`, and
`windows_backend_missing_cache_grant_on_retry_refuses_without_third_boot`.
