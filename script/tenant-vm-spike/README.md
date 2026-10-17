# Tenant VM measurement spike (operator only)

This is a disposable, direct **official Firecracker/jailer 1.17.0** experiment,
not a helper or a tenant-image implementation. Never run the privileged scripts
through the job runner. An operator must use a dedicated Linux x86_64 KVM host.
The tools do not install host packages, change the host firewall, touch production
VMs, or delete tenant disks. Supply checksum-verified `FIRECRACKER` and `JAILER`
absolute paths; the Firecracker executable must be named `firecracker`.

## Inputs and budget

- Root, `/dev/kvm`, `/dev/net/tun`, unified cgroup v2 with CPU/memory/PID
  controllers, an unused jailer uid (default 290000), and reflink-enabled XFS.
- Host tools: Python 3.10+, iproute2, util-linux (including `mount`, `nsenter`,
  `fallocate`), xfsprogs, e2fsprogs, Git, debootstrap, curl, GNU make, GCC,
  flex, bison, bc, OpenSSL development headers, and libelf development headers.
  Kernel and host kernel-build tools use **GCC 13.4.0** (`gcc-13`, not the
  default GCC 15). The builder logs its version and refuses any other version;
  GCC 15's C23 default rejects this kernel's `bool`/`false` definitions.
- The rootfs builder reads `/etc/os-release` for the operator host's **Ubuntu
  release**, recording its suite, packages/build log and runner SHA. This avoids pairing a host-built
  executable with an older glibc. It refuses systemd older than 257 and checks
  that the supplied runner actually executes in the chroot. The guest includes
  distro Rust/Cargo and a native C toolchain: startup always probes `rustc`, and
  isolation compiles native launchers even when the submitted job is a shell.
- The kernel builder pins Linux 6.1.128 and its kernel.org SHA-256. It builds
  XFS, virtio-blk/net/vsock, namespaces, cgroups and nftables into the ELF kernel;
  it uses ordinary pages, not hugepages. It enables cgroup BPF for systemd's
  device/firewall policies and disables tunnel drivers that create fallback
  interfaces inside otherwise private network namespaces. The resolved config
  is checked before compilation and again when saved, not just requested via
  `scripts/config`. Inputs/config/digests are retained.
- Allow **128 GiB peak incremental disk** including Cargo/kernel build scratch,
  rootfs copies, fully allocated 1/20/24 GiB XFS disks and two 18 GiB memory
  snapshots. Allow 22 GiB available host RAM and four build CPUs. Once inputs
  are built, the probe allocates at most 89 GiB: 45 GiB of tenant/placeholder
  disks, 36 GiB of snapshot memory, and two 4 GiB rootfs copies. Its **96 GiB**
  measurement budget adds 7 GiB for state/logs, fixtures and margin; existing
  build inputs already reduce the host's reported free space. The measurement
  preflight requires **240 GiB free** (128 GiB fleet refusal floor + 16 GiB
  reserve + 96 GiB probe budget); each snapshot checks the floor again. Do not
  run if concurrent work could consume that reserve. Typical wall time budget:
  **30–60 minutes including builds**, **5–15 minutes for measurements**; these
  are planning allowances, not measured results.

## Operator commands

Run from this checkout's root; all artifacts stay under it. Install missing host
prerequisites as an operator first. No prerequisite install is hidden in a probe.
The builders refuse existing image/source outputs. Use a fresh results/work name
for each measurement attempt; never reuse a live VM's images.

```sh
mkdir -p script/tenant-vm-spike/.work script/tenant-vm-spike/.results
export CARGO_TARGET_DIR="$PWD/script/tenant-vm-spike/.work/cargo-target"
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= \
  cargo build --release --locked -p motor-runner -j4
CC=gcc-13 JOBS=4 bash script/tenant-vm-spike/build-kernel.sh \
  "$PWD/script/tenant-vm-spike/.work/kernel"
sudo env MOTOR_RUNNER="$CARGO_TARGET_DIR/release/motor-runner" \
  bash script/tenant-vm-spike/build-rootfs.sh \
  "$PWD/script/tenant-vm-spike/.work/image"
# Set FIRECRACKER and JAILER to the checksum-verified official 1.17.0 tool paths.
export FIRECRACKER=/absolute/operator/tool/path/firecracker
export JAILER=/absolute/operator/tool/path/jailer
sudo env FIRECRACKER="$FIRECRACKER" JAILER="$JAILER" \
  python3 script/tenant-vm-spike/hosted_spike.py \
  --kernel "$PWD/script/tenant-vm-spike/.work/kernel/vmlinux" \
  --rootfs "$PWD/script/tenant-vm-spike/.work/image/rootfs.ext4" \
  --work "$PWD/script/tenant-vm-spike/.work/vm-01" \
  --results "$PWD/script/tenant-vm-spike/.results/run-01"
```

Return `summary.json`, `raw.jsonl`, both build logs and any failing VM's console
log. Preserve the results outside `target/`. Output logs include every command,
API body/status, sample, guest probe result, input digest, and the exact launch
command. A failure stops the experiment, writes verdict `replan` (not passing)
in `summary.json` and the last stdout line, and exits nonzero;
partial samples are not promoted into five-run evidence. Precondition failures
are reported on stderr before creating any VMs. SIGINT/SIGTERM and exceptions
stop VMMs, unmount their binds, delete experiment taps/network namespaces and
remove experiment cgroups. Failed cleanup names the retained work directory;
an operator must inspect it. SIGKILL/power loss cannot run a cleanup handler.
Disk images and snapshot artifacts are intentionally retained for inspection;
remove the ignored `.work/` directory only after cleanup succeeded and results
have been retained. They are disposable spike data, never production disks.

## What is measured (five iterations, no warm-up discarded)

The rootfs has systemd PID 1, `motor-job`/`motor-cache` accounts, ACL/sudo/mount
tools and a separate trusted daemon account. `spike-runner.service` runs
`serve --backend host` with isolation **enabled**, tenant state on XFS, max
weight 2, 5 GiB sccache and a 2 GiB free-space floor. Real protocol jobs assert
the `motor-job` identity, hidden guest-root files, and loopback-only networking,
and require an exit-0 terminal plus an output marker. No compiler/cache workload
or outbound-job test is implied. The permissive daemon sudoers entry and the
root vsock test agent are for this throwaway image only, not production policy.
The daemon owns `/tenant/state/workspaces`, bind-mounted at `/Users` before
runner startup. Jobs use `/Users/spike/probe` under the runner's default owned
workspace root, matching the production same-filesystem layout. `/Users` is
not an unrelated root-owned directory and the runner's checks are unchanged.
Before asserting the interface list is exactly `[(1, "lo")]`, each job prints
its interfaces, network namespace identity and `ip -br link`. A short diagnostic
pause lets the root agent capture the job unit's effective namespace/device/BPF
properties, guest/job network namespace IDs and relevant systemd journal lines.
The host records the saved post-`olddefconfig` feature flags and config digest;
extra interfaces remain a failure, not an exception added to the probe.

- `cold_boot`: launch jailer through base-agent vsock readiness. The base has a
  1 GiB placeholder `/dev/vdb`, **not mounted**, and the runner is not started
  until tenant attach. `vsock`: an additional control round trip through
  Firecracker's UDS `CONNECT 54\n`; guest vsock port 52 bridges the host probe's
  length-prefixed JSON job frames to `/tenant/state/spike/runner.sock`.
- `base_snapshot_create`: `PATCH /vm {state:Paused}`, then `PUT /snapshot/create`
  with `snapshot_type:Full`, `snapshot_path`, `mem_file_path`; includes fsync of
  both files and their directory. The guest is synced before pause. Snapshot
  creation is timed separately from preparation/stop; `snapshot_timing` raw
  events separate the API phase (including memory-file writes) from **each**
  subsequent memory-file, VM-state-file and directory fsync. VMM `/proc/PID/io`
  deltas and memory-file length/allocated bytes are also recorded. API timing
  includes Firecracker's internal serialization, not just pure write syscalls;
  no tracing overhead is added to the five-run latency measurements.
- `base_restore_{20,24}g`: new jailer/cgroup, `PUT /snapshot/load` using
  `mem_backend:{backend_type:File,backend_path}`, `resume_vm:false`,
  `enable_diff_snapshots:false` and
  `network_overrides:[{iface_id:eth0,host_dev_name:tap-b}]`; update drive, resume,
  mount `/dev/vdb` and wait for the real runner hello. Includes new-jailer setup
  and readiness, not just the HTTP 204.
- `network_override`: change from tap-a to tap-b, configure the guest's new
  `/30`, ping its new gateway and require tap-b RX packets to increase. Both
  taps are in an experiment-only network namespace, with no uplink/egress or
  production routes. `arp_ignore=1` prevents weak-host ARP from masking a wrong
  tap. These documentation-range addresses never leave that namespace.
- `drive_substitute_{20,24}g`: restore with the placeholder, bind the chosen
  fully allocated XFS file at `/replacement.img`, and
  `PATCH /drives/tenant {drive_id:tenant,path_on_host:/replacement.img}` **before
  resume**. The guest must see exactly 20/24 GiB on `/dev/vdb` and mount XFS.
  `disk_attach_*` times the guest mount, state setup and runner startup;
  substitution/bind time is separately reported. `ficlone_*` invokes Linux
  `FICLONE` directly between two separate bind mounts of that filesystem and
  verifies nonempty random content and distinct inodes, with no copy fallback.
- `suspend`: a 20 GiB disk, an actual **10 GiB resident file page cache** verified
  by `mincore`, guest sync, pause, full snapshot + fsync, VMM stop, bind unmount
  and a `/proc/*/fd` inode scan proving no process holds the disk. The rootfs
  image is kept consistent with the VM state; this is not yet the read-only
  overlay-root implementation. `suspend_snapshot_create` isolates the API/fsync
  portion. The 10 GiB allocation is not used as a proxy for page residency.
- `suspend_restore`: new jailer/cgroup, ordinary-file snapshot load with the
  network override, resume, agent readiness and runner hello. The cgroup uses
  `(18432 + 512) MiB memory.max`, `cpu.max=100000 100000`, `pids.max=66`.
  `restore_memory_peak_bytes` is the fresh restore cgroup's peak through runner
  readiness. A **separate**, untimed full cache-file read checks persistence
  and records peak after touching all cached pages; it is not hidden in the
  resume-to-ready threshold. Another real isolated job must succeed.
- `disk_detach`: stop VMM, wait, unmount binds, scan open fds; the file remains.
  Firecracker has no runtime block hot-unplug operation here. Neither guest
  virtio device deletion nor a nonexistent `DELETE /drives` is claimed.

The last line is **go** only if every required metric has five samples, all
functional probes succeeded, and suspend restore p50 is **<=5 s**. Any transport,
drive, network or FICLONE failure, missing run or >5 s p50 is **replan**. Extra
memory headroom is also a replan, not a silent cap change. Record the operator's
real data and exact command in hosted-runners design section 6 before advancing.

Portable plumbing tests (not VM qualification):

```sh
python3 --version
python3 script/tenant-vm-spike/test_hosted_spike.py
bash -n script/tenant-vm-spike/build-kernel.sh script/tenant-vm-spike/build-rootfs.sh
```
