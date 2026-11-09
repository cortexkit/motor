# Linux microVM components

Linux hosted VMs use the root helper and the unprivileged tenant runner:

| Crate | Binary | Role |
| --- | --- | --- |
| `motor-vm-helper` | `motor-vm-helper` | Root service that starts tenant and job VMs through the official Firecracker jailer, owns disks, cgroups, virtual network interfaces (TAPs) and cache credentials, and returns relay descriptors. |
| `motor-vm-loader` | `motor-vm-loader` | Static, unprivileged loader that serves a VM snapshot's memory through `userfaultfd`. |
| `motor-runner` | `motor-runner` | Runs preparation and systemd-sandboxed commands inside tenant VMs; job VMs restrict the same runner to one job and workspace. |

`motor-host` defaults to `mode: "tenant"`. Its opt-in `mode: "per_job"` schedules
one cold-booted VM per job without memory snapshots or warm VM holds. The image
recipe does not yet produce the job `rootfs.ext4`/`toolchain.ext4`, and no
production path writes the guest's `/run/motor/runner-args`, so per-job mode is
not ready for an end-to-end operator switch.

The standalone loader is not used by a runner backend. `serve --backend vm`
remains unavailable; hosted per-job scheduling is a `motor-host` mode, not that
runner backend. Windows jobs use the backend in [windows-jobs.md](windows-jobs.md).

## motor-vm-helper

The helper is a Linux-only root service. `motor-host` stays unprivileged and
sends bounded identities and operations, not arbitrary paths, executables,
environment or argv.

### Host prerequisites

The paths below are fixed by this build. `/var/lib/motor-vm` is the helper's
storage root and has no override; executable paths come from the
JSON configuration. It is a bind mount of the root-owned `/srv/build/vm`
directory on the XFS build disk (or a dedicated XFS filesystem mounted at
`/srv/build/vm` on a tenant-only host). All ancestors of the mounted view are root-owned;
`/srv/build` itself may remain owned by the unprivileged build account. Root VM
services must use the mounted view, not the source spelling. Renaming the source
from its writable parent does not move an already established bind mount. The
`motor-vm-storage-private.service` companion makes the protected view recursively
private after the bind is mounted; a shared bind would otherwise propagate
chroot mounts back into the source alias. The helper requires that companion,
and `script/tenant-image/qualify.py` refuses a shared view before starting a VM.

- Linux with cgroup v2, `openat2`, pidfds and `cgroup.kill`,
  and XFS with reflink support backing `/srv/build/vm`.
- systemd 254 or newer (for `DelegateSubgroup=`). The unit is a root system
  service with the cpu, cpuset, memory and io controllers delegated. The helper
  process sits in the unit's `helper` leaf, not the delegation root.
- The official Firecracker 1.17.0 jailer and Firecracker executable. The
  executable's basename must stay `firecracker`.
- All binaries, config and image files and their ancestors root-owned, not
  group- or other-writable, and not symlinks.
- A VM uid range (`uid_base`) with no passwd or Name Service Switch (NSS)
  entries, not overlapping the configured `job_account_range`.

### Install

```sh
cargo build -p motor-vm-helper --release --locked
sudo install -m 0755 target/release/motor-vm-helper /usr/local/libexec/motor-vm-helper
sudo install -m 0644 crates/motor-vm-helper/systemd/motor-vm-helper.service /etc/systemd/system/
sudo install -d -o root -g root -m 0755 /srv/build/vm /var/lib/motor-vm
# The unit's name must be the systemd escape of /var/lib/motor-vm; the checkout
# keeps it as motor-vm-storage.mount because Windows file names can't hold \.
sudo install -m 0644 crates/motor-vm-helper/systemd/motor-vm-storage.mount '/etc/systemd/system/var-lib-motor\x2dvm.mount'
sudo install -m 0644 crates/motor-vm-helper/systemd/motor-vm-storage-private.service /etc/systemd/system/
# Write /etc/motor-vm-helper.json, using crates/motor-vm-helper/fixtures/helper.json
# as the shape: allowed uid, socket group, CPU topology and job-account range.
# Set its required uplink and public_ipv4 to this host's actual interface/address;
# install the same settings and outbound policy with install-job-network.py
# --apply --uplink IFACE --public-ip IPV4 before starting the helper.
sudo systemctl daemon-reload
sudo systemctl enable --now 'var-lib-motor\x2dvm.mount'
sudo systemctl start motor-vm-storage-private.service
systemctl is-active motor-vm-storage-private.service
findmnt --noheadings --output PROPAGATION /var/lib/motor-vm
mountpoint /var/lib/motor-vm
sudo systemctl enable --now motor-vm-helper
```

The propagation query must report `private`, not `shared`. Install the companion
before activation; the mount wants it, and `BindsTo=` stops it if the bind goes
away so a later activation reapplies privacy. Apply storage-unit changes only
after stopping/draining VMs, not during a qualification run.

The helper unit requires the mounted view through `RequiresMountsFor=`. Install
and activate the mount before image builds or provisioning as well. Existing
root-managed configs or tenant registries that name the old source path need an
offline operator update before the new helper starts; the helper does not accept
two spellings or migrate live disk records automatically. Do not change ownership
of the whole `/srv/build` tree to work around the helper's trust checks.

Tenant removal is cleanup, not a network admission. It fences the selected TAP
and deletes only its helper-owned chains/counter, membership and comment-owned
UFW grant, using the grant's saved direction even if the uplink is missing.
Interrupted removal replays that cleanup before finishing saved records. Normal
policy reconciliation preserves healthy ready gates; new and unready gates stay
closed until final uplink validation. Failure of either the initial or final
uplink validation fences every
affected gate. Rules bind to interface names;
a same-name replacement inherits them, and root-only interface renames are
outside this boundary.

An invalid or missing uplink leaves every tenant gate fenced, but does not stop
the helper from binding its socket. The ordinary `remove_tenant` request still
works for stopped tenants and resumes an interrupted removal when its registry
is gone. Status, stopping and other operations that do not open a gate remain
available. `ensure`, `restore`, `prepare_start` and `set_egress` return a typed
`start_refused` with case `uplink` and the validation problem. Filesystem trust,
identity, cgroup cleanup and inability to apply the fence remain startup errors;
a failed fence is not evidence that serving is safe.

Startup does not acquire `provision.lock`: image qualification holds that lock
while launching its helper. Removal and resume acquire it within the helper's
request serialization. Resume checks the saved home device/inode and live
TAP-index ownership before mutation. If another trusted tenant owns the index,
network cleanup is refused but the departed tenant's original records and home
are completed data-only, allowing seven-day purge without touching the new
owner. Unreadable ownership records or a different home refuse completion.
Older journals without a saved home identity need explicit manual recovery;
the helper must not guess which same-name directory they describe.
Quarantine records do not reserve indexes during provisioning:
tenant names and home existence do not identify an incarnation. A durable
`complete` marker prevents a finished removal with no home from being replayed
against a later same-name tenant, and permits normal seven-day purge.
Tenant-home and quarantine parents are checked and opened component-by-component
without symlinks; home movement uses descriptor-relative rename rather than
following a changed parent spelling.

Portable regressions are
`invalid_or_missing_uplink_binds_fences_and_serves_cleanup_but_refuses_opening`,
`qualification_lock_allows_helper_startup_and_reproduces_legacy_lock_failure`,
`reassigned_resume_completes_only_original_data_without_touching_replacement_network` and
`completed_homeless_removal_never_revives_for_a_replacement_and_can_be_purged`.
These use real sockets/flocks and injected network commands, not privileged
kernel/UFW or VM qualification.

The tenant deny is applied before parsing or withdrawing host delegation. If
the registry is unreadable, the helper recovers TAP names from the live tenant
table and closes those gates; the wildcard guard also denies unknown TAPs. If
network containment fails, it hard-stops the running VMMs instead of relying on
a refused admission to contain existing traffic. If that containment also fails,
the helper exits so systemd tears down its delegated service cgroup; it must not
continue serving with uncontained VMMs. Producer reloads retain every
pre-existing `closed_taps` member: only a helper reconciliation with fresh uplink
and UFW validation can release a fence. The helper and producer serialize those
policy changes on the same network lock.

`TenantMetrics.egress_bytes` is an absolute per-tenant-incarnation byte counter.
Provisioning resets an existing per-index counter before publishing a new
registry, and Python removal resets it before releasing that allocation. Rust
removal deletes it; routine reconciliation never resets a live tenant's counter.
Motor-host records a fresh per-job baseline and reports a saturating delta, so
index reuse cannot inherit the former tenant's bytes or produce a negative bill.

The socket is `/run/motor-vm-helper.sock`, `root:<socket_gid>`, mode `0660`.
Only uids listed in the config are admitted, checked with `SO_PEERCRED`.

Verify with `systemctl status motor-vm-helper`. To remove it, drain and stop
every client instance, then `systemctl disable --now motor-vm-helper`.
Persistent images may stay. Never remove a live
VM's jail or cgroup by hand. Keep `uid_base` unchanged while the service is
stopped: processes that escaped from an old uid range cannot be identified
under a new one.

### Job VM interface and lifecycle

The helper accepts bounded, newline-delimited JSON operations from authorized
peers. A job start names `job_id`, `account`, `workspace_disk_id` and
`image_generation_id`, with `vcpus`, `memory_mib`, optional `burst_mib` and a
`kind` of `job` (default), `fill`, `base_build` or `recovery`. It accepts neither
caller paths nor key material. Relays return descriptors; the host never gets
the Firecracker API socket. Tenant operations are described in the
[helper README](../crates/motor-vm-helper/README.md#persistent-tenant-operations).

Job VMs have the following contracts. The named portable tests exercise the
implementation with fixtures; root-only tests need a dedicated operator host.

1. **Cold boot, not snapshot restore.** Each job VM gets its requested vCPUs and
   sized RAM, a read-only root, a read-only account toolchain on virtio-pmem,
   and one writable workspace on virtio-blk. Virtio-pmem presents a file-backed
   persistent-memory device; direct access (DAX) maps those toolchain pages
   without a second guest page-cache copy. There is no bundle drive: bundles
   use the host-to-runner channel. Image generations are immutable and pinned
   at admission; later publication does not replace files in use. Toolchain
   copies use full physical blocks, not reflinks shared across accounts. Tests:
   `cold_boot_uses_request_size_and_exactly_the_three_job_images`
   (`crates/motor-vm-helper/src/linux/job/tests.rs`),
   root-only `isolated_publication_verifies_pins_and_seals_acl_ready_physical_account_images`
   (`crates/motor-vm-helper/src/linux/images/tests.rs`),
   `generations_pin_at_claim_across_publication_gc_attachment_and_restart`
   (`crates/motor-host/tests/it/job_admission/storage/images.rs`).
2. **One disk attachment.** Workspace identity is `(account, workspace hash)`.
   Same-workspace jobs may be admitted and queued, but no disk is attached to
   two VMs. The host never mounts, parses or runs Git on guest-written disks.
   Tests: `disk_identity_uses_account_and_hash_ignoring_cold_and_foreign_hints`
   (`crates/motor-host/tests/it/job_admission.rs`) and
   `attached_disk_is_refused_by_identity_and_inode_until_released`
   (`crates/motor-vm-helper/src/linux/job/tests.rs`).
3. **Same job sandbox.** The tenant runner in a job VM performs checkout,
   sibling/dependency preparation, read-only source sealing and returned files.
   Before runner start, per-execution records, events, output and returned-file
   history are cleared; mirrors, workspace catalog, snapshot pins and retirement
   metadata remain warm. The toolchain must mount with DAX before the workspace
   is mounted. Root and containers inside commands are not provided. Sandbox
   test: `isolated_job_vm_preserves_full_sandbox_contract`
   (`crates/motor-host/tests/it/job_contract.rs`), which checks all of
   [job-isolation.md 4.4.2](job-isolation.md#4-processes) in a real job VM. Naming
   this operator test does not establish that it has passed. Cleanup tests:
   `job_vm_cleanup_pins_only_execution_history_and_preserves_warm_state` and
   `job_vm_cleanup_never_follows_history_symlinks`
   (`crates/motor-runner/src/tenant_agent/paths.rs`); DAX test:
   `daxless_boot_refuses_before_workspace_mount_or_account_input`
   (`crates/motor-runner/src/tenant_agent/per_job.rs`).
4. **Memory and accounting.** Guest RAM is
   `ceil((mem_mib + burst) / 0.75)` MiB; stated memory stays `mem_mib`, while the
   job's MemoryMax includes burst. Admission debits stated memory plus boot
   reserve until runner hello, never burst. Pressure control uses host-measured
   guest-RAM residency excluding pmem; guest samples are audit-only. Tests:
   `guest_ram_rounds_limit_up_after_contract_burst_not_admission_debit` and
   `queued_and_booting_debits_end_once_on_hello_not_on_boot`
   (`crates/motor-host/tests/it/job_admission.rs`). Host virtual machine monitor
   (VMM) cgroups and out-of-memory (OOM) evidence are specified in the
   [helper README](../crates/motor-vm-helper/README.md#job-vm-cgroups-and-restarts).
5. **Durable replay before power-off.** The host persists runner frames and
   serves callers from its replay store even when detached. The guest can power
   off only after the terminal is acknowledged as stored. Host restart adopts
   the VM and resumes from the stored sequence, dropping duplicates. Tests:
   `detached_caller_retains_raw_output_returns_terminal_and_one_usage_across_poweroff_and_restart`
   and `host_down_while_command_finishes_resumes_last_sequence_and_drops_duplicate_output`
   (`crates/motor-host/src/server/job/tests/replay_tests.rs`). Bounds and retention
   are in the [host README](../crates/motor-host/README.md#per-job-replay-store).

### Per-job host settings and rollback

The helper grants the Fill guest operation only from the root-owned disk role,
never from a host-supplied start kind alone. Job VMs refuse the Fill operation,
and Fill VMs refuse Run admission. Fill VMs import verified bundles on a fresh
disk, never a filesystem written by a job VM. Current per-job fills are
cache-only and delete their disks instead of promoting
[build bases](compile-cache.md#fixed-paths-and-local-build-bases) that seed later jobs.
Promotion requires fixed paths, which per-job hosts do not have yet. See
[per-job fill sources](hosted-host.md#per-job-fill-sources) for the byte and time
limits and VM resource lifetime.

The host JSON's required `per_job` settings have no compiled-in defaults:

| Setting | Purpose |
| --- | --- |
| `vcpu_ceiling` | Maximum job vCPUs, bounded by online CPUs. |
| `permit_budget` | Overcommitted host vCPU permits, from the ceiling through four times online CPUs. |
| `host_reserve_mib` | Available-memory reserve in MiB. |
| `boot_reserve_mib` | MiB added to each pre-hello admission debit. |
| `per_vm_overhead_mib` | MiB above stated memory before resident RAM counts as overage. |
| `reclaim_timeout_ms` | Balloon-reclaim wait before rechecking pressure. |
| `hysteresis_mib` | MiB above the reserve required before balloon deflation. |
| `workspace_disk_cap_bytes` | Sparse disk cap and growth reservation bound. |
| `boot_deadline_ms` | Boot-to-runner-hello deadline. |
| `recovery_timeout_ms` | Recovery-start deadline, followed by disk deletion on timeout. |
| `exit_evidence_bound_ms` | Bound from observing a gone VMM to missing-evidence settlement. |
| `teardown_timeout_ms` | Failed guest-channel teardown bound. |

Time settings are milliseconds. Parsing and the startup refusal are covered by
`per_job_mode_refuses_each_unset_setting_without_defaults` and
`complete_per_job_config_is_typed_but_startup_refuses_unavailable_runtime`
(`crates/motor-host/src/main.rs`). The executable still refuses per-job startup.

The helper owns the account credential store. Root's `motor-vm-helper
set-remote-cache` installs a complete set atomically; only future starts see a
replacement. Jobs get read credentials, fills/base builds also get the write key,
and recovery gets none. See the [root command and source-file requirements](hosted-host.md#root-remote-cache-credential-command).

The implemented rollback keeps placement drained, selects tenant mode and
restarts from retained tenant disks through recovery, discarding per-job workspace
disks only after their VMs stop. The helper must install its current credential
set into each tenant VM before placement can resume. Failure leaves the host
drained; saved tenant-disk keys never override a rotation. Tests:
`rotate_then_rollback_installs_all_current_sets_before_undrain` and
`failed_rollback_install_and_restart_keep_placement_fenced`
(`crates/motor-host/tests/it/rollback.rs`). See
[hosted-host.md](hosted-host.md#per-job-mode-rollback); it does not prescribe
enabling per-job mode while its startup gate is closed.

## motor-vm-loader

Build it as a static binary, because it runs in an empty root:

```sh
cargo build -p motor-vm-loader --release --target x86_64-unknown-linux-musl --locked
```

The standalone loader accepts `motor-vm-loader --memory-fd N --listen-fd N`. Both are
inherited descriptors, not paths: a read-only snapshot-memory file and a bound
Unix listener. The loader accepts one Firecracker connection, receives exactly
one `userfaultfd` and a bounded JSON region list, then closes the listener.

- The regions must cover the memory file completely with contiguous,
  non-overlapping ranges. Every address, size and offset is 2 MiB aligned, every
  page size is 2 MiB, and no arithmetic may overflow.
- Only missing-page events are accepted. Each fault is served with one 2 MiB
  `UFFDIO_COPY`; a partial copy is fatal, and `EEXIST` is a harmless race.
- The loader needs no pathname access, file writes or new sockets. Its sandbox
  test uses no supplementary groups, an empty root, an unprivileged uid,
  `no_new_privs` and a syscall allowlist. The helper has no loader launch path.

## Tests

The portable suites run on macOS or Linux without KVM:

```sh
cargo nextest run -p motor-vm-helper --lib --bins --locked
cargo nextest run -p motor-host --lib --bins --locked
cargo nextest run -p motor-vm-loader --locked
```

The feature-gated lanes fail, rather than skip, when a prerequisite is missing.
They run as root, create mounts, cgroups and systemd units, and use fixed uids,
so run them only on a disposable trusted Linux host, never in hosted CI or a
build job. Prefix each with the Git-config clearing from
[CONTRIBUTING.md](../CONTRIBUTING.md#prerequisites).
The service lane also requires `/etc/motor-network.json` from the explicit-uplink
network installer and that interface UP with its configured public IPv4 address;
it reconciles real host policy rather than using the portable fixture's example.

| Command | Needs | Covers |
| --- | --- | --- |
| `sudo ... cargo test -p motor-vm-helper --features linux-root-tests --lib` | Root and `CAP_SYS_ADMIN`, scratch on a native Linux filesystem | Filesystem, credential, mount-resolution and descriptor handling |
| `sudo ... cargo nextest run -p motor-vm-helper --features linux-service-tests --lib` with `MOTOR_VM_TEST_JAILER` and `MOTOR_VM_TEST_FIRECRACKER` | Root, systemd 254+ as PID 1 with delegation, cgroup v2, unused uid/gid 200000, the official jailer, and the static fixture built from `fixtures/firecracker.c` | Cgroup placement, restart cleanup, and the real jailer layout |
| `cargo nextest run -p motor-vm-helper --features linux-service-tests --lib -E 'test(=linux::job::tests::isolated_job_vmm_survives_helper_service_restart)'` | Root, systemd, cgroup v2, pinned jailer and static fixture, exclusive fixture uid | Job VMM slice, OOM score, restart adoption and exit evidence; not a real guest boot |
| `cargo nextest run -p motor-host --lib -E 'test(=tests::job_contract::isolated_job_vm_preserves_full_sandbox_contract)' --run-ignored only --no-tests fail` | Dedicated root/KVM host and the test's explicit real-job-VM fixture | Full in-job sandbox contract in 4.4.2 |
| `cargo test -p motor-vm-loader --features linux-uffd-tests --locked` | `userfaultfd` permission | Real `UFFDIO_COPY` of 2 MiB pages |
| `MOTOR_VM_LOADER_BIN=<static loader> cargo nextest run -p motor-vm-loader --features linux-root-tests --locked` | Root and the helper's seccomp support | The loader reaching its fault loop under the real sandbox |

Build the jailer-layout fixture with
`cc -static -O2 -o target/helper-fixture/firecracker crates/motor-vm-helper/fixtures/firecracker.c`.
It is a stand-in that records its uid and root, not a VMM. Do not run two copies
of the service lane at once: both use the same transient unit name and fixture
uid.

`cargo nextest list -p <crate> --features linux-root-tests --locked` compiles
those tests without privileges. Benchmarks: `cargo bench -p motor-vm-loader --bench copy --locked`
measures the layout and copy logic; add `-- --uffd` on Linux to measure real
`UFFDIO_COPY` calls.

Portable fixtures do not prove real guest boot, DAX, virtual-socket (vsock)
transport, network isolation or in-VM systemd behavior. Those require the root/KVM operator lanes
with prepared images; no skip counts as a pass.
