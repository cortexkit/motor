# Linux microVM components

Three crates implement the parts of a Firecracker-based Linux job backend:

| Crate | Binary | Role |
| --- | --- | --- |
| `motor-vm-helper` | `motor-vm-helper` | Root service that owns a pool of VM slots, starts and stops Firecracker through the official jailer, and hands the runner only file descriptors. |
| `motor-vm-loader` | `motor-vm-loader` | Static, unprivileged loader that serves a VM snapshot's memory through `userfaultfd`. |
| `motor-vm-guest` | `motor-vm-guest` | Supervisor that runs as root inside the guest image and executes one command per VM. |

**No runner backend uses them in this build.** `motor-runner` does not depend on
these crates, and `serve --backend vm` fails at startup with "VM backend is not
available in this build" (`crates/motor-runner/src/backend.rs`, `select`).
Installing the helper does not change how any runner instance runs jobs. Linux
jobs run in the systemd sandbox described in [job-isolation.md](job-isolation.md);
Windows jobs run in the separate QEMU backend described in
[windows-jobs.md](windows-jobs.md).

This document records the interfaces between the three parts and how to test
them.

## motor-vm-helper

The helper is a Linux-only root service. The runner stays unprivileged and
never sends the helper a path, executable, environment or argv.

### Host prerequisites

The paths below are fixed by this build. `/srv/build/vm` is the helper's storage
root and has no override; executable and snapshot paths come from the JSON
configuration.

- Linux with cgroup v2, `openat2`, pidfds, `cgroup.kill` and `/dev/userfaultfd`,
  and XFS with reflink support at `/srv/build`.
- systemd 254 or newer (for `DelegateSubgroup=`). The unit is a root system
  service with the cpu, cpuset, memory and io controllers delegated. The helper
  process sits in the unit's `helper` leaf, not the delegation root.
- The official Firecracker 1.17.0 jailer and Firecracker executable. The
  executable's basename must stay `firecracker`.
- A static `motor-vm-loader` (below).
- All binaries, the config, snapshot files and their ancestors root-owned, not
  group- or other-writable, and not symlinks. Shared vmstate and rootfs inputs
  readable by the slot uid (normally mode `0444`).
- A slot uid range (`uid_base`, default 200000) with no passwd or NSS entries,
  not overlapping the job-account range; set `job_account_range` to the job
  backend's real reservation.
- A reserved hugepage pool. `hugepages_per_slot` is the number of 2 MiB pages
  for one guest.

### Install

```sh
cargo build -p motor-vm-helper --release --locked
sudo install -m 0755 target/release/motor-vm-helper /usr/local/libexec/motor-vm-helper
sudo install -m 0644 crates/motor-vm-helper/systemd/motor-vm-helper.service /etc/systemd/system/
sudo install -d -o root -g root -m 0755 /srv/build/vm
# Write /etc/motor-vm-helper.json, using crates/motor-vm-helper/fixtures/helper.json
# as the shape: allowed uid, socket group, CPU topology and job-account range.
sudo systemctl daemon-reload
sudo systemctl enable --now motor-vm-helper
```

The socket is `/run/motor-vm-helper.sock`, `root:<socket_gid>`, mode `0660`.
Only uids listed in the config are admitted, checked with `SO_PEERCRED`.

Verify with `systemctl status motor-vm-helper`. To remove it, drain and stop
every client instance, then `systemctl disable --now motor-vm-helper` and stop
the hugepage reservation. Persistent images may stay. Never remove a live
slot's jail or cgroup by hand. Keep `uid_base` unchanged while the service is
stopped: processes that escaped from an old uid range cannot be identified
under a new one.

### Protocol

Newline-delimited JSON, at most 4096 bytes per request. Unknown operations and
fields are refused.

```json
{"op":"acquire","instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001"}
{"op":"start","slot":0,"instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001","kind":"job"}
{"op":"stop","slot":0,"instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001","export":true}
{"op":"release","slot":0,"instance":"e2e","job_id":"00000000-0000-4000-8000-000000000001"}
{"op":"list","instance":"e2e"}
```

- `acquire` returns `{"result":"acquired","slot":0}`, or `slot: null` when none
  is free. The lease belongs to the connection and is identified by slot,
  instance and job UUID.
- `stop` does not release the lease; `release` requires a stopped VM. A lease
  whose connection drops becomes dirty and is freed only after cleanup succeeds.
- If the loader or jailer dies, the helper revokes the lease, kills every slot
  process and cleans the slot without export. The agent stream then ends; the
  runner must treat that as an abnormal end, not a guest exit status.
- `start` returns `{"result":"started","fd_count":N}` with the descriptors
  attached to the first reply byte (`SCM_RIGHTS`). Descriptor 0 is the agent's
  vsock stream; descriptor 1, when present, is the proxy listener. The runner
  never receives the Firecracker API socket.

### Job files

Job directories are `/srv/build/vm/<instance>/jobs/<UUID>/`, owned by the
client's uid. Instance names match `[a-z0-9-]{1,32}`.

- Inputs are the fixed files `tree.in`, `target.in` and `cargo.in`. The helper
  checks their ownership and sizes against the snapshot record's `image_sizes`
  and reflinks them into the jail as `/tree`, `/target` and `/cargo`. A scan
  reads only `tree.out`.
- Outputs are created exclusively, never overwritten: a job exports `tree.out`
  and `target.out`, a warm build `target.out`, a fetch `cargo.out`, a scan
  nothing. Export only after the guest reports a clean status, and remove
  consumed outputs before another VM exports the same names.
- All job-directory access runs as the client's uid (`setfsuid`/`setfsgid`)
  through `openat2` with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS` and
  `RESOLVE_NO_XDEV`.
- Size refusals log `vm_image_size`; capacity refusals log `vm_capacity`.

The slot cleanup boundary is `/srv/build/vm/jail/slot-<n>`, root-owned mode
`0700`. The Firecracker root follows the jailer's layout:
`/srv/build/vm/jail/slot-<n>/firecracker/slot-<n>/root`. Cgroups are
`<unit>/slots/slot-<n>`.

### Restart

On start, before publishing its socket, the helper kills every slot-uid process
(including escaped ones), unmounts everything under each jail, removes jails and
cgroups, and marks all slots free. If any of that fails, startup stops rather
than publishing an unsafe slot. A cleanup failure at runtime leaves the slot
dirty, and maintenance retries it. Stop the unit to kill its whole cgroup.

## motor-vm-loader

Build it as a static binary, because it runs in an empty root:

```sh
cargo build -p motor-vm-loader --release --target x86_64-unknown-linux-musl --locked
```

The helper runs `motor-vm-loader --memory-fd N --listen-fd N`. Both are
inherited descriptors, not paths: a read-only snapshot-memory file and a bound
Unix listener. The loader accepts one Firecracker connection, receives exactly
one `userfaultfd` and a bounded JSON region list, then closes the listener.

- The regions must cover the memory file completely with contiguous,
  non-overlapping ranges. Every address, size and offset is 2 MiB aligned, every
  page size is 2 MiB, and no arithmetic may overflow.
- Only missing-page events are accepted. Each fault is served with one 2 MiB
  `UFFDIO_COPY`; a partial copy is fatal, and `EEXIST` is a harmless race.
- The loader needs no pathname access, file writes or new sockets. The helper
  starts it with no supplementary groups, in an empty root, inside the slot
  cgroup, as the slot uid, with `no_new_privs` and a syscall allowlist.

## motor-vm-guest

`motor-vm-guest [--port 52]` runs as root inside the guest image. It accepts one
vsock connection per restored VM; the host sends one `Start` and receives
`Frame`s. The types in `crates/motor-vm-guest/src/lib.rs` are the wire schema:
a big-endian u32 length followed by strict JSON, at most 1 MiB. Output frames
carry raw bytes, separately for stdout and stderr. A VM runs exactly one
session and must then be stopped.

### Image requirements

- Fixed virtio drives: `/dev/vda` read-only root, `/dev/vdb` tree, `/dev/vdc`
  target, `/dev/vdd` Cargo home; data drives unmounted when the snapshot is
  taken.
- `/proc`, `/sys`, `/dev` and a writable `/run` mounted by the image's init.
- A job user with uid and gid 1000, no `sudo` or setuid-root binaries, and only
  loopback networking.
- The real pinned Cargo at `/opt/motor/bin/cargo` (not a rustup proxy), Git at
  `/usr/bin/git`, and an immutable `/etc/motor-vm/cargo-config.toml`.
- The image's own `HOME`, `PATH`, `RUSTUP_HOME` and similar keys; request
  overrides of them are discarded.

### Command execution

Each `trees` entry maps a direct child of the tree image to an absolute path
strictly below `/job`; the client translates workspace, sibling, cwd and
manifest paths into that root. Traversal, symlink components and overlapping
destinations are refused. Data mounts are `nosuid,nodev`. The target and Cargo
mounts are `/run/motor-vm/target` and `/run/motor-vm/cargo`.

Before running the command the agent sets the clock, credits 256 bits of host
entropy and reseeds the kernel RNG, mounts the drives, and starts the network
relay if `proxy_port` is set. The command drops groups, gid and uid, starts its
own session and sets `no_new_privs`. Afterwards the agent kills every remaining
uid-1000 process with pidfds, drains output, reaps, syncs and unmounts. Only
then does it send `Status { clean: true }`. Any failure closes the stream
without a clean status, and the host must not export that VM's images.

With `proxy_port`, the relay listens on `127.0.0.1:3128` and opens a new vsock
connection to the host for each TCP connection. HTTP policy belongs to the host
proxy. Without it the job has no relay and is forced offline.

### Dependency fetch

A fetch ignores the request's environment and runs only the image's
`cargo fetch --locked` from `/run/motor-vm/fetch` with the requested manifest;
it never runs workspace code. `git_directories` maps each locked Git source and
revision to a directory prepared from the client's Git bundles, and Git
`insteadOf` rules route those sources to local repositories. The host rejects
unavailable sources before launch; the guest rejects missing or extra mappings.

Before reporting a clean status, the agent walks `CARGO_HOME` and rejects
symlinks, special files and unknown paths. It checks every registry archive
against the lockfile's SHA-256, requires an archive for each extracted source,
and checks each Git checkout's revision. The only extra metadata accepted is
Cargo's standard `CACHEDIR.TAG` in `registry/` and `git/`.

## Tests

The portable suites run on macOS or Linux without KVM:

```sh
cargo nextest run -p motor-vm-helper --lib --bins --locked
cargo nextest run -p motor-vm-guest --locked
cargo nextest run -p motor-vm-loader --locked
```

On Linux, `cargo test -p motor-vm-helper` also runs the hugepage admission tests
(`linux::tests::hugepage_shortage_refuses_before_vm_preparation`,
`linux::tests::sufficient_hugepages_proceed_with_vm_preparation`,
`linux::tests::unreadable_hugepage_pool_refuses_before_vm_preparation`). They
inject the free-page count and need no root or hugepages.

The feature-gated lanes fail, rather than skip, when a prerequisite is missing.
They run as root, create mounts, cgroups and systemd units, and use fixed uids,
so run them only on a disposable trusted Linux host, never in hosted CI or a
build job. Prefix each with the Git-config clearing from
[CONTRIBUTING.md](../CONTRIBUTING.md#prerequisites).

| Command | Needs | Covers |
| --- | --- | --- |
| `sudo ... cargo test -p motor-vm-helper --features linux-root-tests --lib` | Root and `CAP_SYS_ADMIN`, scratch on a native Linux filesystem | Filesystem, credential, mount-resolution and descriptor handling |
| `sudo ... cargo nextest run -p motor-vm-helper --features linux-service-tests --lib` with `MOTOR_VM_TEST_JAILER` and `MOTOR_VM_TEST_FIRECRACKER` | Root, systemd 254+ as PID 1 with delegation, cgroup v2, unused uid/gid 200000, the official jailer, and the static fixture built from `fixtures/firecracker.c` | Cgroup placement, restart cleanup, and the real jailer layout |
| `cargo nextest run -p motor-vm-guest --features linux-root-tests --locked` | Root, `CAP_SYS_ADMIN`, util-linux `unshare`, mount/PID/network namespaces | Privilege drop, mounts, outside-path hiding, reaping escaped processes |
| `cargo nextest run -p motor-vm-guest --features trusted-cargo-tests --locked` | Network access and a real Cargo (not a rustup proxy) on `PATH` | Real registry and local-Git fetches |
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

Snapshot restore, the guest clock and entropy calls, the ext4 mount lifecycle,
vsock transport and loader seccomp compatibility need real VMs on a KVM host
with prepared images and reserved hugepages. No test in this repository covers
them.
