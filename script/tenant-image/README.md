# Tenant image

`build.py` builds a shared, immutable **tool image**, not a tenant disk. It uses
the JSON document `versions.txt`; it never discovers versions from the operator
host during a build. Rebuilding the inputs under another version name is
supported; byte-identical filesystem images are not promised.

## Inputs

The release toolchain inventory was captured on the Linux build server on
2026-10-17. `stable` resolved to Rust 1.99.0 and is recorded as that release, not
as a floating channel. Each release has its own installed target list. Node
24.21.0, npm 11.19.0, Bun 1.4.2 and pnpm 11.1.2 came from the same inventory.
To deliberately refresh those pins, run on the tool-supplying Linux server:

```sh
python3 script/tenant-image/capture-versions.py
```

Review and commit the refreshed `versions.txt` before building. Capturing refuses a pnpm version
other than 11.1.2. It preserves the separately reviewed kernel, OS, package and
installer pins. There is no automatic capture in the build path.

The other inputs are:

- Linux 6.1.128 with a recorded kernel.org SHA-256, built with GCC 13.4.0.
  `kernel-options.txt` requests built-in XFS (including reflink), overlayfs,
  ext4, virtio block/network/vsock and the systemd job-isolation prerequisites.
  The resolved configuration, not just the requested options, is checked.
- Debian trixie from the dated Debian snapshot in `versions.txt`. Packages,
  including Python 3, C/clang/lld, SQLite, mingw win32, binaryen and hivex, are
  resolved from that signed snapshot. Systemd must be at least 257 for
  `PrivatePIDs`. Node/npm come from the pinned upstream distribution rather
  than the distribution's Node package.
- SHA-256-pinned copies of the runner's `tools.txt`, `install-tools.sh`,
  `build-sccache.sh`, read-only-hit patch and `motor-net.nft`. A changed source
  file fails the build until its pin is deliberately updated. Every Cargo tool
  is installed by the existing installer, which uses locked release versions
  or full Git revisions. Sccache is built by the patched builder, not replaced
  by stock `cargo install sccache`.
- An explicitly supplied Linux `motor-runner` binary. Its SHA-256 is recorded
  in the manifest. Use a binary built from the intended reviewed Motor commit
  and record that commit in the operator's release record. The build executes
  the binary inside the guest chroot, so an incompatible libc/architecture
  fails rather than producing a silently unusable image. A static musl binary
  avoids depending on the build host's libc. Parse-only refusal probes require
  `tenant-agent` and `--tenant-vm` without starting either service.

Rustup, Node and Bun downloads are checked against their published SHA-256s
over HTTPS. Rustup checks the pinned toolchain downloads. npm installs exact
npm/pnpm versions with lifecycle scripts disabled. A reachable public registry,
the public pinned Git tools and the historical Debian snapshot are necessary
for a full build. No credentials or adjacent checkout are inputs.

## Operator build

Full image builds require root on a **dedicated Linux x86_64 operator host**;
do not run them through the unprivileged remote-job lane. First install and
activate the [root-trusted VM storage mount](../../docs/linux-microvm.md#install).
The default output root is `/var/lib/motor-vm/tenant-images`; do not use the
`/srv/build/vm` source spelling for root-run operations. The host needs
debootstrap with trixie support and current Debian archive signing keys,
e2fsprogs, util-linux, Python 3, curl, xz, make, GCC 13.4.0 (`gcc-13`), flex,
bison, bc, libssl-dev and libelf-dev. Allow at least 200 GiB of scratch space
for the tool builds plus the 32 GiB rootfs. Cargo invocations within this build
are serial; `--jobs` caps each build's parallelism. Both Rust compiler-wrapper
environment variables are explicitly cleared, including in the chroot.

```sh
sudo python3 script/tenant-image/build.py build EXAMPLE_VERSION \
  --runner /absolute/path/to/motor-runner --jobs 4
python3 script/tenant-image/build.py inventory \
  /var/lib/motor-vm/tenant-images/EXAMPLE_VERSION
```

The version directory is reserved with an exclusive `mkdir` **before** checking
the binary, downloading or building anything. Even a concurrent invocation
cannot overwrite a version. A failed build deliberately leaves its reserved
directory and `.work` for diagnosis; `build.py inventory` rejects any directory
that still contains `.work`. Before manually
removing a failed version to retry, check that no mounts remain under `.work`.
Never recursively delete a chroot with live bind mounts. Successful builders
unmount first, remove scratch, then write `manifest.json` as their completion
record. Existing completed versions must never be removed to rebuild in place.

The build creates the following artifacts under the operator-host output root,
`/var/lib/motor-vm/tenant-images/<version>/`, where `<version>` is the chosen release name:

- `vmlinux`, `rootfs`: read-only files; rootfs is an ext4 **lower**, not the
  tenant's XFS data filesystem;
- `manifest.json`: version, exact parsed `versions.txt`, kernel/rootfs/runner
  SHA-256s, and the required kernel boot arguments;
- `versions.txt`, `kernel.config`, `packages.txt`, `tools.json`: input pins,
  resolved kernel options, installed package versions and executed tool probes.

The image contains systemd, `motor-job`/`motor-cache` nologin accounts, sudo/ACL
and namespace prerequisites, Python 3 and `motor-runner`. The existing tools
installer's build-only directories live inside the disposable chroot; only
`/usr/local/lib/motor/cargo/bin` and `/usr/local/lib/motor/rustup` enter the image.
Registry, Git, build and compile caches are not baked into the shared image.
Firecracker/jailer and the kernel compiler are host-only prerequisites from
`tools.txt`, not guest VM-launching tools.

## Boot and tenant attachment

Attach `rootfs` as **read-only `/dev/vda`** and use `manifest.json.boot_args`.
Do not boot straight into systemd: `init=/sbin/motor-init` mounts a 1 GiB tmpfs,
overlays the read-only lower, pivots into it, and execs systemd as PID 1. The
read-only original root remains at `/.lower`; the RAM upper/work directories
remain under `/overlay`. No initrd or on-disk kernel modules are needed.
Machine ID, hostname, resolver configuration and systemd state are not a
tenant identity baked into the disk. Fresh starts get a new RAM upper; suspend
snapshots retain that upper. This setup must precede a per-size base snapshot.

`motor-tenant-agent.service` runs as root, loads the VM's `motor_net` rules and
then listens on vsock. The rules use `eth0` (`net.ifnames=0`) instead of the
host installer's physical-interface name. IPv4 forwarding is enabled, kernel
module loading is disabled, and generic firewall/update/network services are
masked; only the agent configures tenant networking. No SSH login is installed.
`systemd-remount-fs.service` is masked as well: the init script has already set
up both roots, and the kernel's `ro` flag must not remount the RAM overlay read-only.

The agent receives `CARGO_HOME=/usr/local/lib/motor/cargo` as the **immutable
bind source**, plus the image's `RUSTUP_HOME`, `PATH`, and `HOME=/tenant`. It
mounts the provisioned XFS `/dev/vdb`, binds its tool `bin` read-only onto
`/tenant/cargo-home/bin`, binds `/tenant/roots/{Users,home}`, and runs the
FICLONE probes. It starts the transient `motor-tenant-runner` unit using
`/tenant/runner-args`; that runner gets `CARGO_HOME=/tenant/cargo-home`. The image
does not enable a second runner service or supply tenant-specific arguments.
The placeholder `/dev/vdb` must stay unmounted until `attach_tenant`.

Provisioning owns the XFS disk, argument file, tenant identity/network values,
per-size snapshots and Firecracker lifecycle. This directory implements none
of the helper's tenant-disk cleanup or snapshot state machine.

## Checks

Portable checks (no root, no VM, no network):

```sh
python3 -W error script/tenant-image/test_tenant_image.py
bash -n script/tenant-image/build-kernel.sh script/tenant-image/build-rootfs.sh
sh -n script/tenant-image/guest/motor-init
```

Resolve the actual pinned kernel configuration without compiling a kernel
(unprivileged Linux, network, compiler prerequisites; output must not exist):

```sh
python3 script/tenant-image/build.py kernel-config /absolute/scratch/kernel-config
```

The portable tests cover immutable-version refusal through the real CLI,
repeat builds into different names, real artifact hashing, pin drift, failed
build reservations, actual resolved-config rejection, capability-probe error
handling and the agent's environment. Fake artifacts in these tests are **not**
evidence of a built or booted production image. Invoke these tests directly;
the central script-test registry is outside this directory's edit scope.

On the dedicated VM host, first run artifact `inventory`. To qualify a real
boot, add `systemd.wants=motor-image-check.service` to the manifest boot args
and capture its console result. That opt-in service verifies systemd PID 1,
the writable overlay, read-only ext4 lower, exact 1 GiB tmpfs limit, accounts,
active agent/rules, loaded `motor_net`, private writes and denied lower writes.
It is not enabled on normal boots. After tenant attachment, an operator can
also run `/usr/local/libexec/hosted_image_check.py --tenant` inside the guest
to probe XFS FICLONE between `/tenant/state` and both bound workspace roots.
These live-guest checks require the dedicated operator host and must not be
counted as passing when skipped.

## Operator qualification

`qualify.py` is an operator-only lane for an already built image. Run it as root
on the dedicated Linux KVM server, **not** through the remote-job runner. It
refuses an existing helper socket, registered tenants, tenant jails, tenant taps,
tenant firewall table or base-boot network namespaces. It will not stop or
replace an installed helper. Stop here if the host already serves tenant VMs;
use a dedicated qualification host instead. Ordinary non-VM fleet jobs need not
be stopped, but schedule the snapshot window off peak.

Build the current checkout's real host/helper/fake-broker binaries and a Linux
runner **library test executable** first. The test executable is delivered to
the guest; neither Cargo sources nor a new runner binary are installed over the
image's runner. Build the test executable in the image's own Debian/GNU-libc
userland with `build-guest-test.py`, rather than assuming the host's glibc is old
enough. The locked `cortexkit-cow` dependency does not currently compile for
musl, so a musl test build is not a preparation path. An incompatible or absent
test executable is a **FAIL**, never a skip.

The preparation tool mounts the original rootfs read-only under a disposable
host overlay, binds this source checkout and the populated host Cargo registry
read-only, and compiles offline with the image's Rust/C toolchains. Compiler
wrappers and inherited environment are cleared. It lists the required test in
that same chroot to verify the loader/library combination, saves the executable
and provenance, and unmounts everything before removing the overlay. This is a
trusted root build environment, not a sandbox for untrusted source. It does not
run the isolation test there; the exact ignored test still runs inside the real
VM during qualification. No dependency pin or lockfile change is needed.

```sh
BUILD="$PWD/target/tenant-qualification"
mkdir -p "$BUILD"
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= CARGO_TARGET_DIR="$BUILD" \
  cargo build --locked -j4 -p motor-host -p motor-vm-helper -p motor-lease --bins
IMAGE=/var/lib/motor-vm/tenant-images/IMAGE_VERSION
PREP=/var/lib/motor-vm/qualification-prep/UNUSED_PREP_NAME
sudo python3 script/tenant-image/build-guest-test.py --image "$IMAGE" \
  --source "$PWD" --cargo-cache "${CARGO_HOME:-$HOME/.cargo}" --output "$PREP" --jobs 4
# Select two host CPUs excluding any CPUs reserved for Windows jobs.
sudo python3 script/tenant-image/qualify.py \
  --image "$IMAGE" \
  --bin-dir "$BUILD/debug" --runner-test-binary "$PREP/runner-tests" \
  --cpuset 0-1 --results /var/lib/motor-vm/qualification/UNUSED_RUN_NAME
```

Prerequisites are Python 3.9+, systemd 257+ (including `systemd-debug-generator`
and binary kernel credentials), unified delegated CPU/cpuset/memory/pids/I/O
controllers, XFS with reflink, OpenSSL with Ed25519, Git, xfsprogs, util-linux,
iproute2, nftables and the pinned Firecracker/jailer 1.17.0. No packages or host
users are installed. The admission server runs as `nobody` by default; choose a
different existing unprivileged account with `--host-user` if necessary.

Budget, **not measured results**: allow 5–15 minutes after builds, approximately
53 GiB peak incremental disk plus the delivered executable/logs, and about
17 GiB host RAM. Builds can add 5–20 minutes and their own Cargo scratch. The
chroot preparation requires 232 GiB free (170 GiB floor + 54 GiB qualification
reserve + an 8 GiB build allowance), without copying the 32 GiB rootfs. Its target
files, `runner-tests`, `cargo-artifacts.jsonl` and `provenance.json` remain under
`--output`; remove them only after confirming preparation's cleanup succeeded.
The qualification requires **224 GiB free on `/var/lib/motor-vm` after builds**: a 170 GiB floor
plus a 54 GiB allocation allowance. It rechecks the floor plus snapshot bytes
before suspend. It uses 2 vCPUs / 16 GiB guest RAM, a fully allocated 20 GiB XFS
disk, a 16 GiB size base and one 16 GiB suspend image. Snapshot `memory.high` is
14 GiB only during the helper's paused snapshot window; I/O weight is 10.
These settings are a qualification workload, not production capacity approval.

The source release is never changed: an exclusive temporary image version
hard-links its read-only kernel/rootfs and copies the manifest with boot-only
extra arguments. Provisioning builds a new size base from `manifest.boot_args`.
The systemd debug generator receives `motor-qualification.service` as a binary
kernel credential and wants it only on this boot. Its kernel-command-line
condition is a second opt-in fence. A boot-only drop-in sends the existing image
check directly to ttyS0, avoiding journald prefixes on exact evidence lines.
The unit waits for real tenant attachment, runs `hosted_image_check.py --tenant`
and the delivered `isolation::tests::hosted_job_cannot_open_vsock --ignored
--exact` test as root, and later checks for failed guest units. There is no
privileged guest-exec RPC, SSH service, or normal-boot qualification unit.

The harness starts the real `motor-vm-helper` in a delegated transient unit and
the real TLS `motor-host` admission server. A separate `motor-lease` fake broker
issues a requester-bound lease for each job. The fake broker's seed is never
passed to the host. TLS SPKI pinning, the challenge signature, the runner Hello,
Git bundle upload, output frames, nonzero exit 23, second-job target path/inode
and sentinel reuse are all checked. The helper performs suspend/restore with
its normal quiesce/freeze/thaw and durable-snapshot implementation; fallback to
a fresh base fails qualification. A third leased job proves persistence after
restore. This tests the production admission/launch path with a fake **control
plane**, not a real broker deployment or a billing qualification.

Every step prints PASS/FAIL and raw evidence; unattempted steps are FAIL. The
guest checker's complete success lines must equal `qualify.py`'s `LIVE` and
`TENANT` constants, including the parenthesized check lists. These are the
literal lines printed by `guest/hosted_image_check.py`, not substring matches. Output, manifests and summary stay under
`--results`, outside Cargo target directories; credentials are not logged.
Teardown stops the guest through the helper before stopping its unit, removes
only disposable qualification resources, and prints process/tap/firewall/mount/
failed-unit evidence. Existing unrelated failed host units are reported as a
baseline, not cleared. Cleanup errors fail qualification and identify retained
paths; never recursively delete them while mounts or VMMs remain. SIGINT and
SIGTERM trigger teardown; SIGKILL or host loss cannot do so.

Return `summary.json`, `raw.jsonl`, `console.log`, `base-console.log`, the
host/fake-broker logs and the complete `base-boot/` directory. Base-boot evidence
includes exact submitted API bodies and responses, the observed Firecracker VM
configuration, separate VMM stderr, a Debug-level Firecracker log, flushed metrics,
cgroup memory/CPU/I/O counters, and process status before and after cleanup. A
host-issued SIGKILL is recorded separately from a VMM that already exited. Failure
rows always include an exception type, nonempty message and traceback; captured
subprocess failures retain their real exit status/stdout/stderr. The terminal
prints bounded log tails while full artifacts remain on disk.

Base boot retries only transport failures before the initial `CONNECT 54`
handshake completes, for at most 60 seconds. Firecracker can close such an early
connection while the guest vsock device exists but its agent has not started.
Once a guest control request is submitted, EOF or a refusal is fatal, not retried;
a repeated prepare request could hide an unknown operation outcome. A readiness
timeout names the last transport failure and whether the VMM was still running.
**No live qualification result is claimed by the portable tests or by merely
building this harness.**

Portable harness tests:

```sh
python3 -W error script/tenant-image/test_qualify.py
python3 -W error script/tenant-image/test_build_guest_test.py
python3 -W error script/tenant-provision/test_provision.py
```
