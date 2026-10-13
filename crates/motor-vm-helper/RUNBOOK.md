# motor-vm-helper: privilege boundary and no-KVM gates

This Linux-only root service owns the machine-wide slot pool. The runner remains
unprivileged. It never sends a path, executable, environment, or argv to the
helper. No production backend switch is enabled by this crate.

## Install prerequisites

The paths below describe the layout required by this build, not an installed
host inventory. `/srv/build/vm` is the helper's fixed storage root; it has no
runtime path override. Executable and snapshot paths in the JSON configuration
are configurable. Slot allocation uses `uid_base`, configurable with a default
of 200000; choose a free range large enough for the configured slots.

- Linux with cgroup v2, `openat2`, pidfds, `cgroup.kill`, `/dev/userfaultfd`, and
  XFS with reflink support at `/srv/build`.
- systemd **254 or newer** (`DelegateSubgroup=` was introduced in 254). The unit
  must be a root system service, with cpu, cpuset, memory and io delegated. The
  helper is in the unit's `helper` leaf, not the delegation root. Both the unit
  root and `slots` must have empty `cgroup.procs` before enabling controllers.
- The official **Firecracker 1.17.0 jailer** and Firecracker executable. Keep the
  executable's basename `firecracker`. Do not substitute a custom jailer.
- A **static** Rust UFFD loader implementing
  `motor-vm-loader --memory-fd <fd> --listen-fd <fd>`. It inherits only its
  read-only snapshot-memory fd, the bound UFFD listener, stderr and standard
  null descriptors. The helper executes it by fd after dropping supplementary
  groups, entering an empty root, entering the slot cgroup, dropping uid/gid,
  setting no_new_privs and installing a syscall allowlist. The loader must not
  need pathname I/O, fork, threads, or network sockets. It validates the UFFD
  handshake and uses the received UFFD; this crate does not implement the loader.
- All binaries, config, snapshot files, and their ancestors must be root-owned,
  not group/other-writable and not symlinks. Shared vmstate/rootfs inputs must be
  readable by the slot uid (normally mode 0444). Snapshot memory need not be.
- The slot uid range must have no passwd/NSS entries and must not overlap the
  job-account backend range. Set `job_account_range` to that backend's actual
  reservation; the example is not an allocation of job accounts.
- Reserve the hugepage pool before enabling VM jobs. `hugepages_per_slot` is the
  2 MiB page count for one guest (8192 for 16 GiB).

Build with `cargo build -p motor-vm-helper --release`. Install the binary to
`/usr/local/libexec/motor-vm-helper` and the unit from `systemd/` to
`/etc/systemd/system/`. Create `/srv/build/vm` root-owned mode 0755. Configure
`/etc/motor-vm-helper.json` using `fixtures/helper.json` as a shape example,
changing allowed uid, socket group, CPU topology and the job-account range to
match the host. The socket is `/run/motor-vm-helper.sock`, root:socket_gid, 0660.
Only allowed SO_PEERCRED uids are admitted. `systemctl daemon-reload` then
`systemctl enable --now motor-vm-helper` enables the service; it does not enable
VM execution in any runner instance.

## Runner/image/loader interface

The stream protocol is newline-delimited JSON, at most 4096 bytes per request.
Unknown operations and fields are refused. The operation shapes are:

```json
{"op":"acquire","instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001"}
{"op":"start","slot":0,"instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001","kind":"job"}
{"op":"stop","slot":0,"instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001","export":true}
{"op":"release","slot":0,"instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001"}
{"op":"list","instance":"e2e"}
```

Acquire returns `{"result":"acquired","slot":0}` or `slot:null`. The lease
belongs to this connection and is identified by slot, instance and parsed UUID.
Stop does not release it. Release requires a stopped VM. A disconnected lease
becomes dirty before cleanup, cannot be acquired, and becomes free only after
cleanup succeeds. Loader/jailer death revokes a running lease and cleans its
slot without export. All slot processes are killed, ending the agent stream;
runner code must interpret this as abnormal end, not a guest Exit status.

Start returns `{"result":"started","fd_count":1}` with SCM_RIGHTS on the
first reply byte. FD 0 is the connected agent vsock stream; FD 1, when present,
is the proxy listener (fetch, or job with proxy_for_jobs). Receivers must use
`recvmsg`, validate the descriptor count, and handle a partial stream reply.
The runner never receives the Firecracker API socket.

Jobs live at `/srv/build/vm/<instance>/jobs/<canonical-UUID>/`, owned by the peer
uid. Instance names match `[a-z0-9-]{1,32}`. Job/fetch/warm inputs are fixed files
`tree.in`, `target.in`, `cargo.in`; scan reads only `tree.out`. The helper checks
regular-file ownership and sizes from the selected snapshot record's
`image_sizes` map (`tree`, `target`, `cargo`). It reflinks these into the jail as
`/tree`, `/target`, `/cargo`. Snapshot drive paths must use these names, plus
`/rootfs`. Scan's snapshot must have only its read-only tree data drive. The
snapshot UDS path must be `/vsock.sock`. The snapshot memory is not put in the
jail; `/uffd.sock` connects Firecracker to the fd-based loader. The rootfs and
`/vmstate` are read-only single-file bind mounts preserving their shared page
cache. The runner validates the *complete* manifest/record/qualification before
asking to start; the helper validates sizes independently. Size refusals log
`vm_image_size`, capacity refusals `vm_capacity`.

Output names are exclusive creations, never overwrites: job exports `tree.out`
and `target.out`, warm exports `target.out`, fetch exports `cargo.out`, scan
exports nothing. The runner should request export only after a clean guest
status, and remove consumed outputs before another VM exports the same names.
All job-directory resolution and output creation runs with peer setfsuid and
setfsgid and openat2(BENEATH|NO_SYMLINKS|NO_XDEV). An opened job dir stays pinned
through start or stop, and replacement is checked again before completion.

The slot cleanup boundary is `/srv/build/vm/jail/slot-<n>`, created root-owned
mode 0700 so the runner cannot reach the API or UFFD sockets by pathname.
**The actual Firecracker root** follows the official jailer's layout:
`/srv/build/vm/jail/slot-<n>/firecracker/slot-<n>/root`. Its loader's empty root
is `loader-root` inside that root. Process-root checks must use these actual
paths, not the cleanup boundary. Cgroups remain exactly
`<unit>/slots/slot-<n>` (no controller enabled inside a populated slot leaf).

## Verification without KVM

Run from the worktree root. All test sources are unit modules; there is no new
integration entry point to bypass the repository's folded-test gate.

Portable contract/model tests (not Linux acceptance):

```sh
cargo nextest run -p motor-vm-helper --lib --bins
```

On Linux, the default unprivileged suite also runs
`linux::tests::hugepage_shortage_refuses_before_vm_preparation`,
`linux::tests::sufficient_hugepages_proceed_with_vm_preparation` and
`linux::tests::unreadable_hugepage_pool_refuses_before_vm_preparation`:

```sh
cargo test -p motor-vm-helper
```

These tests inject the free-page reader into the same preparation gate used by
`start`; production still reads `HugePages_Free` from `/proc/meminfo`. A shortage
must return `vm_capacity: not enough HugePages_Free` before invoking any VM
preparation. The injected continuation creates real jail/chroot directories and
a marker, proving it is not invoked on shortage or read failure, but does run
at the exact required page count and above it. They require no root, KVM,
snapshots or hugepage reservation. The existing root-only tests do not test
hugepage capacity; they cover ownership, mounts, service placement and cleanup.

Linux container-runnable filesystem, credentials, mount-resolution and FD tests
(no systemd, jailer or KVM required). Needs **root and CAP_SYS_ADMIN** for the
cross-device mount test; keep scratch on a native Linux filesystem, not a macOS
shared mount that virtualises chown. The explicit feature runs the privileged
tests and fails if its prerequisites are missing, rather than silently skipping.

```sh
sudo env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
  CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" PATH="$PATH" \
  "$(command -v cargo)" test -p motor-vm-helper --features linux-root-tests --lib
```

The real no-KVM **service acceptance feature** adds the systemd cgroup placement,
restart cleanup (stale jail, cgroup and escaped slot-uid process), and official
jailer layout tests. Needs root, systemd >=254 running as PID 1 with cpu/cpuset/
memory/io delegated, writable cgroup v2, and unused uid/gid 200000. These tests
use 200000 as a fixed fixture, independent of the configurable production
`uid_base` default. It creates a
transient unit `motor-vm-helper-placement-test` with Delegate=yes,
DelegateSubgroup=helper, KillMode=control-group. Do not run two copies of this
feature concurrently or run it on a host already using uid 200000 for jobs.
The service test checks kernel placement, not a mocked cgroup tree.

Build the no-KVM *static layout fixture* on Linux (it is not shipped as a VMM):

```sh
mkdir -p target/helper-fixture
cc -static -O2 -o target/helper-fixture/firecracker crates/motor-vm-helper/fixtures/firecracker.c
export MOTOR_VM_TEST_JAILER=/usr/local/libexec/jailer
export MOTOR_VM_TEST_FIRECRACKER="$PWD/target/helper-fixture/firecracker"
"$MOTOR_VM_TEST_JAILER" --version # must be the official 1.17.0 release
sudo env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  MOTOR_VM_TEST_JAILER="$MOTOR_VM_TEST_JAILER" \
  MOTOR_VM_TEST_FIRECRACKER="$MOTOR_VM_TEST_FIRECRACKER" \
  RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
  CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
  PATH="$PATH" "$(command -v cargo)" nextest run \
  -p motor-vm-helper --features linux-service-tests --lib
```

The layout test invokes the real jailer against the fixture, then checks the
root-observed artifact written by the executed, uid-dropped process. It does
not merely compare the helper's formula to an identical expected formula.
No test in this feature opens KVM or needs hugepages/snapshots. Real restore,
loader seccomp compatibility and end-to-end guest tests require separate real-VM
acceptance tests on Linux with KVM, the static loader, prepared snapshots and
guest images, and reserved hugepages. They are outside the portable and no-KVM
suites.

## Restart and failure recovery

Stop the helper unit to kill its entire cgroup, not individual helper/VM PIDs.
On restart, before the socket is published, it kills all configured slot-uid
processes (including escaped processes), reaps direct children, unmounts all
mounts beneath each jail boundary, removes the jail/cgroup, reconciles leftover
slot indices after a pool shrink and marks all slots free. Failure to kill,
unmount or remove stops startup, rather than publishing an unsafe free slot.
A runtime cleanup failure leaves a dirty slot and maintenance retries it.

Keep uid_base stable while the service is stopped: processes escaped from an
old, changed uid range cannot be identified by the new config. Drain and stop
all instances before changing the slot uid allocation. Rolling back the runner
to its non-VM backend does not require deleting persistent images; after drain,
stop and disable the helper and hugepage reservation unit. Never manually
remove a live slot's jail or cgroup.
