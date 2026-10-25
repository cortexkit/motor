# Tenant image

`build.py` builds a shared, immutable **tool image**, not a tenant disk. It uses
the JSON document `versions.txt`; it never discovers versions from the operator
host during a build. Rebuilding the inputs under another version name is
supported; byte-identical filesystem images are not promised.

## Inputs

`script/runner/toolchain.json` is the job-visible toolchain contract for the
build server, Boat and tenant images. The tenant recipe calls Boat's tool
installer with the tenant role, including the separate frozen `stable` copy,
exact Rust components/targets, upstream NATS, GNU aliases and pinned runtime
archives. The legacy release fields in `versions.txt` are still validated
(including its pnpm 11.1.2 constraint), but do not select installed tools.
`capture-versions.py` only refreshes that legacy summary. To propose a
toolchain refresh, capture on the
tool-supplying Linux server and review the shared manifest:

```sh
python3 script/runner/toolchain-parity.py --capture > toolchain-state.json
python3 script/runner/toolchain-parity.py --from-capture toolchain-state.json \
  > toolchain-proposed.json
```

Review the proposed shared manifest, source archives and per-role differences,
then refresh both images' `runner_inputs_sha256` maps. Tenant pins also cover
the shared Boat installer, parity checker, account installer and inotify sysctl.
There is no automatic capture in the build path.

The other inputs are:

- Linux 6.18.58 with a recorded kernel.org SHA-256, built with GCC 13.4.0.
  Descriptor-relative cleanup requires `fchmodat2(AT_EMPTY_PATH)`, introduced
  in Linux 6.6; the 6.1 series cannot provide it through a config change. The
  minimum is 6.18 to stay on a [Firecracker-supported guest kernel series](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/kernel-policy.md)
  with that syscall; 6.12 is not a supported series.
  `kernel-options.txt` requests built-in XFS (including reflink), overlayfs,
  ext4, virtio block/network/vsock and the systemd job-isolation prerequisites.
  The resolved configuration, not just the requested options, is checked.
  All nftables inet reject variants and IPv4/IPv6 reject helpers are required
  built-in: defconfig otherwise leaves reject expressions modular, which cannot
  work with the locked, module-store-free guest. Counter, metadata, payload and
  interval-set expressions are part of `NF_TABLES` itself in Linux 6.18; there is
  no separate `NFT_COUNTER` option at this pin.
  CPU entropy is trusted by default in 6.18; the old `RANDOM_TRUST_CPU` config
  option no longer exists.
  The boot drivers are compared with Firecracker 1.17's
  [validated x86-64 6.18 config](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config).
  `HW_RANDOM` and `HW_RANDOM_VIRTIO` are built in for virtio-rng; `PRINTK`
  preserves boot/panic logs on the 8250 `ttyS0` console. The selected
  `VIRTIO_VSOCKETS_COMMON` transport is checked explicitly. Unlike the ACPI
  validation config, this guest keeps `VIRTIO_MMIO_CMDLINE_DEVICES=y` for its
  existing legacy MMIO boot path. The validation config targets Amazon Linux,
  so its unrelated settings do not replace this image's firewall and isolation
  requirements.
- Debian trixie from the dated Debian snapshot in `versions.txt`. Packages,
  including Python 3, C/clang/lld, SQLite, mingw win32 and hivex, are resolved
  from that signed snapshot. Systemd must be at least 257 for `PrivatePIDs`.
  Node/npm come from the pinned upstream distribution rather than the
  distribution's Node package.
- SHA-256-pinned copies of the runner's `tools.txt`, `install-tools.sh`,
  `build-sccache.sh`, read-only-hit patch and `motor-net.nft`. A changed source
  file fails the build until its pin is deliberately updated. Every Cargo tool
  is installed by the existing installer, which uses locked release versions
  or full Git revisions. Sccache is built by the patched builder, not replaced
  by stock `cargo install sccache`.
- Git built from the build server's release (`toolchain.json`'s `tools.git`:
  kernel.org source with a pinned SHA-256) by the pinned `build-git.sh`, into
  `/usr/local/lib/git/<version>` and linked from `/usr/local/bin`, ahead of
  Debian's older git on the agent's and jobs' PATH. Debian's git package stays
  installed; the build dependencies (libcurl GnuTLS, expat, PCRE2, zlib,
  gettext) are in the package list. Image verification runs the git the agent's
  PATH finds and requires the server's exact `git --version`.
- Binaryen 120 comes from the official upstream x86_64 Linux release archive,
  pinned by SHA-256 in `toolchain.json` and installed under
  `/usr/local/lib/binaryen/120`; the distro `binaryen` package is purged. The
  agent PATH resolves `/usr/local/bin/wasm-opt` to that release and image
  verification requires major version 120, matching the build server.
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
bison, bc, libssl-dev and libelf-dev. The build requires either an explicit
first-build allocated-byte cap (`--build-budget-gib`) or a matching successful
`disk-usage.json` (`--peak-profile`); there is no implicit scratch estimate.
The free-space floor is 170 GiB. A first 64 GiB cap therefore needs 234 GiB free,
not because the build is known to need 64 GiB, but because that is the operator's
admitted limit until a peak is measured. Cargo invocations within this build
are serial; `--jobs` caps each build's parallelism. Both Rust compiler-wrapper
environment variables are explicitly cleared, including in the chroot.

Build the static `motor-runner` on a Linux x86_64 build server before passing it
to the image builder. Install the operator-only `musl-tools` apt package and the
Rust musl target. The runner's `ring` dependency compiles C code: set
`CC_x86_64_unknown_linux_musl=musl-gcc` explicitly because the default
`x86_64-linux-musl-gcc` executable is not provided by `musl-tools`.

```sh
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
command -v musl-gcc >/dev/null || { echo 'musl-gcc is required; install musl-tools' >&2; exit 1; }
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 \
  CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked --target x86_64-unknown-linux-musl -p motor-runner
```

```sh
sudo python3 script/tenant-image/build.py build EXAMPLE_VERSION \
  --runner "$PWD/target/x86_64-unknown-linux-musl/release/motor-runner" \
  --jobs 4 --build-budget-gib 64
python3 script/tenant-image/build.py inventory \
  /var/lib/motor-vm/tenant-images/EXAMPLE_VERSION
```

The build logs checkpoints and writes `disk-usage.json` beside its artifacts on
both success and ordinary failure, with periodic sampled peaks of allocated
file blocks owned by this version (nominal one-second interval plus phase
checkpoints). Sparse holes are not counted as 32 GiB of
real use, hardlinks are deduplicated, symlinks/mounts are not traversed, and
reflink sharing can conservatively overcount blocks. The minimum global free
space is reported separately: activity by other jobs cannot masquerade as the
build's allocated-byte peak. An in-progress profile is refreshed during the build
but cannot admit another build. A complete successful matching profile admits
its peak plus 25%, rounded up to GiB, plus 4 GiB; recipe, runner, input pins and
job count must match. For example, a repeat build can replace the explicit cap
with `--peak-profile /var/lib/motor-vm/tenant-images/PRIOR_VERSION/disk-usage.json`.
The monitor stops only its active build process group on cap/floor/measurement
failure and retains the failed version; SIGKILL or host loss can leave only the
last in-progress sample, never a reusable successful profile.

The version directory is reserved with an exclusive `mkdir` **before** checking
the binary, downloading or building anything. Even a concurrent invocation
cannot overwrite a version. A failed build deliberately leaves its reserved
directory and `.work` for diagnosis; `build.py inventory` rejects any directory
that still contains `.work`. Before manually
removing a failed version to retry, check that no mounts remain under `.work`.
Never recursively delete a chroot with live bind mounts. The rootfs subprocess
runs in a private mount namespace, with its /dev subtree private too. Cleanup
parses that namespace's mountinfo and identifies stage descendants by directory
`(st_dev, st_ino)` ancestry (including the same directory through another bind
alias), not by a mountpoint pathname prefix. It performs blocking, non-lazy
unmounts deepest first, including nested or stacked mounts. It refuses
busy/unchanged mounts by name. The parent checks its own mountinfo before artifact promotion and again
immediately before stage removal; a failed fence retains staging and publishes
no manifest. Successful builders remove scratch, then write `manifest.json` as
their completion record. `python3 build.py check-mounts RETAINED_VERSION` is the
read-only mount fence to run before any manual disposal of an incomplete
version. Existing completed versions must never be removed to rebuild in place.

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
`/usr/local/lib/motor/cargo/bin`, `/usr/local/lib/motor/rustup`, the parity
checker/manifest and sccache build receipts enter the image. Bun lives at
`/usr/local/lib/motor/bun/bin` (the manifest's `tools.bun.path_by_role.tenant`),
not at the build server's `/home/ubuntu/.bun/bin`: in a tenant VM `/home` is the
tenant's own workspace root, bound over the image's `/home` at attach, so
anything the image put there disappears before the runner starts. The recipe
removes `/home/ubuntu` after moving the Rust toolchain out and refuses the image
if anything remains under `/home` or `/Users`. Required packages and Git build
dependencies come from the shared manifest in addition to the tenant OS packages.
Both images create `motor-job` with the manifest's exact UID and primary GID;
numeric collisions refuse the build and name the existing owner. `motor-cache`
remains an image-local, non-job account with a system-assigned identity.
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
masked; only the agent configures tenant networking. `openssh-server` supplies
`/usr/sbin/sshd` for callers' transport tests, but SSH services/socket activation
are masked and generated host keys removed. No SSH login is enabled. The shared
inotify sysctl raises `fs.inotify.max_user_instances` to 8192 at guest boot.
The nft and tenant-agent units forward their own stdout/stderr to the serial
console, so an nft startup failure remains visible even when it prevents the
agent and opt-in qualification unit from starting. Static module-load directories
and the legacy `/etc/modules` list are cleared and checked during image creation;
even an empty `.conf` is removed because it can activate the loader through
`ConditionDirectoryNotEmpty`. The loader is not masked or allowed to fail. Its
own output also forwards to the console if it is unexpectedly activated.
`systemd-remount-fs.service` is masked as well: the init script has already set
up both roots, and the kernel's `ro` flag must not remount the RAM overlay read-only.

The image installer uses one canonical agent-path list for both root:root mode 0755
normalization and build-time component validation: /tenant, /Users, /home and
the image CARGO_HOME/bin. Every ancestor is checked with the agent's real-directory,
uid 0 and no-group/other-write rule. Debian's root:staff mode 2775 /usr/local defaults
are not accepted as immutable tool inputs, even when a version probe runs fine.
The checked path list is recorded in tools.json before the image can complete.

The agent receives `CARGO_HOME=/usr/local/lib/motor/cargo` as the **immutable
bind source**, plus the image's `RUSTUP_HOME`, `PATH`, and `HOME=/tenant`. The
runner does not use HOME or PATH to find its JavaScript tools: `--tenant-vm`
selects the tenant layout, so its trusted dependency setup runs Bun from
`/usr/local/lib/motor/bun/bin` and Node, npm and pnpm from `/usr/local/bin`,
refusing by path if one is missing, and it mounts the Bun directory into jobs at
`/home/ubuntu/.bun/bin`, the tenant job PATH's first entry. Each job still gets
its own `/motor-home` as HOME. The agent
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
python3 -W error script/tenant-image/test_build_storage.py
bash -n script/tenant-image/build-kernel.sh script/tenant-image/build-rootfs.sh
sh -n script/tenant-image/guest/motor-init
```

Resolve the actual pinned kernel configuration without compiling a kernel
(unprivileged Linux, network, compiler prerequisites; output must not exist):

```sh
python3 script/tenant-image/build.py kernel-config /absolute/scratch/kernel-config
```

Toolchain parity runs `script/runner/syscall-surface.py`'s direct-syscall probes
in every role, on the host and in jobs. To check only the syscall contract in
a real job (with reflink-capable `TMPDIR`), run:

```sh
python3 script/runner/syscall-surface.py --check --job
```

The probe must reject an old 6.1 tenant kernel at `fchmodat2.EMPTY_PATH` and
pass after rebuilding and qualifying a new image version. In `--job` mode,
the probe expects `ENOSYS` for `clone3` in every role because PID-init's seccomp
policy deliberately denies it. Outside job mode, the invalid call must reach
the kernel and return `EINVAL`.

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

## Rebuilding guest boot policy

A kernel-feature, module-load policy or guest-unit change requires a **new image
version**, using the normal `build.py build NEW_VERSION --runner ...` path. Do
not replace only the kernel or edit an existing completed rootfs in place: that
would retain old guest-unit/module configuration and break the manifest's artifact
hashes. Reuse the original verified runner binary when no runner code change is
intended; the builder executes it inside the newly built userland as before.
Set an explicit allocated-build-byte cap for the first rebuild, or supply a
successful peak profile matching the recipe, runner, input pins and job count.
After the build completes, `build.py inventory NEW_IMAGE` must verify artifact
hashes and kernel requirements; `qualify.py --image NEW_IMAGE ...` must then run
the real boot, guest checks, leased jobs, suspend/restore and teardown gates.
`build.py check-kernel` rejects configurations with `NFT_REJECT` or its family
companions set to `m`: those modules cannot be loaded in a guest that has no
module store and locks module loading.

## Operator qualification

Image creation runs `toolchain-parity.py --check --role tenant`, including
required packages and sccache build receipts. Qualification reruns that host
check after attachment and runs `--check --role tenant --job` in each leased
job (cold, warm and post-restore). It fails if the checkout's parity inputs
differ from those pinned into the image. The job check covers exact UID/GID,
PATH order, tool versions, Rust components/targets and real libstd payloads,
read-only Rustup, readable proc, writable shared memory, executable TMPDIR,
sshd availability and the inotify instance floor. The versionless `gnutest`
is bound to its package-qualified image hash, rechecked in the booted guest.
Tenant dependency caches live on its private data disk; unlike Boat's fleet
seed, their contents are not part of this immutable toolchain contract.

A passing parity check alone does not show that the runner can prepare a
job: the parity checker finds tools on its own PATH, the runner at its role's
fixed paths. So image creation also runs `motor-runner check-javascript-setup
--tenant-vm`, the runner's own trusted dependency setup with the tenant
runner's tool lookup, on embedded fixtures locked with `bun.lock`,
`package-lock.json` and `pnpm-lock.yaml` (recorded as `javascript_setup` in
`tools.json`); a setup tool missing from where the runner looks fails the
build. Qualification repeats that check in the booted guest after attachment,
when the tenant's `/home` and `/Users` are bound over the image's (`PASS guest
runner JavaScript setup`), and every leased job's repository carries the same
three fixtures, so each job's real workspace setup installs them and the job
fails unless it did.

Qualification also runs `motor-runner check-tenant-cargo-fetch --scratch DIR`
after attachment. It creates a new, empty Cargo home, materializes an embedded
workspace locked to `itoa` 1.0.18, and runs the production trusted online fetch
command in a real snapshot helper unit with the normal 120-second online bound.
There is no offline fallback in this check: it requires a successful online
attempt and the downloaded crate archive in the new cache. The helper has its
own mount namespace, so merely testing DNS in the daemon or a `net` job cannot
substitute for this check. The host requires the exact `PASS guest trusted cargo
prefetch` line as the `trusted cargo fetch` qualification step; a warm persistent
tenant cache cannot make it pass. Portable coverage is
`tenant_online_qualification_requires_a_new_cache_and_a_downloaded_crate` and
`test_trusted_cargo_fetch_runs_online_in_a_helper_with_a_cold_cache`; only the
booted qualification run establishes online reachability.

On a host with a registered tenant, `qualify.py` deliberately refuses. Do not
remove a registration or stop a live helper to bypass this fence. After the
operator deploys the new image through the normal tenant replacement workflow,
verify it with a real job on that tenant, not an operator shell. Obtain the
trusted hash on the operator host:

```sh
python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["parity"]["observed"]["tools"]["gnutest"]["sha256"])' \
  /var/lib/motor-vm/tenant-images/IMAGE_VERSION/tools.json
```

Pass that hash literally to the job (do not compute it from the same job's
`gnutest`, which would compare the file to itself):

```sh
python3 script/runner/toolchain-parity.py --capture --role tenant --job
python3 script/runner/toolchain-parity.py --check --role tenant --job \
  --gnu-test-sha256 OPERATOR_RECORDED_HASH
```

Require exit 0 and `passed: true`. Archive both outputs; a changed recipe does
not prove a rebuilt/deployed image passes. Keep the normal disposable-host
qualification lane for suspend/restore, kernel and isolation evidence.

`qualify.py` is an operator-only lane for an already built image. Run it as root
on the dedicated Linux KVM server, **not** through the remote-job runner. It
refuses an existing helper socket, registered tenants, tenant jails, tenant taps,
tenant firewall table or base-boot network namespaces. `/var/lib/motor-vm` must
be a mount with no shared propagation peer, so child mounts cannot be replicated
back through `/srv/build/vm`. Install `motor-vm-storage-private.service` alongside
the bind unit using [the VM storage install steps](../../docs/linux-microvm.md#install);
it runs `mount --make-rprivate` on that view. Before any VM the harness also
runs `test_mount_alias_root.py` in a private namespace: real bind-alias mounts
must be detected/refused and then removed by the inode-aware cleanup helper.
That privileged check is mandatory, not counted as passing when unavailable.
It will not stop or
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
Install the [explicit host uplink settings and outbound policy](../../docs/runner-install.md#6-optional-job-networking)
first. Qualification reads `/etc/motor-network.json` into its transient helper
config; neither the helper nor the network loader chooses an interface from a
default route. The interface must be UP with the configured public IPv4 address.
Images using the runner's network success-generation protocol must include the
matching `motor_network_ready.py` and guest network unit. The guest publishes
`/run/motor-net-ready` only after its boot-time nft load succeeds; construction
must not publish a marker in the image. Rebuild the image when upgrading from a
table-only runner probe.

Budget, **not measured results**: allow 5–15 minutes after builds, approximately
53 GiB peak incremental disk plus the delivered executable/logs, and about
17 GiB host RAM. Builds can add 5–20 minutes and their own Cargo scratch. The
chroot preparation requires 232 GiB free (170 GiB floor + 54 GiB qualification
reserve + an 8 GiB build allowance), without copying the 32 GiB rootfs. Its target
files, `runner-tests`, `cargo-artifacts.jsonl` and `provenance.json` remain under
`--output`; remove them only after confirming preparation's cleanup succeeded.
The qualification requires **224 GiB free on `/var/lib/motor-vm` after builds**: a 170 GiB floor
plus a 54 GiB allocation allowance. It rechecks the floor plus snapshot bytes
before suspend. It uses 2 vCPUs / 16 GiB guest RAM, a 20 GiB XFS
disk, a 16 GiB size base and one 16 GiB suspend image. Snapshot `memory.high` is
14 GiB only during the helper's paused snapshot window; I/O weight is 10.
These settings are a qualification workload, not production capacity approval.

New builds advertise `sparse-v1` and `resize-v1` in `manifest.json`. The provisioner
must build the size base with tenant-drive `discard: true` and `io_engine: "Sync"`;
old bases must be rebuilt, because drive PATCH on restore cannot enable discard.
The operator lane's `cold discard` and `warm discard` steps each write and fsync
1 GiB in the root guest payload, pause while the host records
`tenants/<name>/disk.img`'s `st_blocks * 512`, then delete, sync and fstrim `/tenant`.
An ordinary sandboxed job only releases the measurement barrier. Each step refuses
unless the host allocation falls by at least 900 MiB. Warm measurement follows a
real helper suspend and restore; a base fallback is a failure. Measurements are
recorded as `discard_measurement` entries in `raw.jsonl`.

On the dedicated operator host, repeat the same command with a fresh results path
and `--without-discard`. This control changes only the tenant drive's base-build
discard bit and must fail `cold discard` (a FAIL is evidence, not a passing
qualification). The control is recorded in `raw.jsonl`; it never modifies the
source release. Neither live run belongs on the shared build runner. Portable
tests check the probe choreography and the exact 900 MiB boundary, not physical
block reclamation.

The guest agent trims completed runner evictions within its 5-second status poll
and also trims after 5 minutes of continuous idleness, including after restore.
Trim failures are logged and retried; trim does not run while the guest filesystem
is frozen for a snapshot.

The host-only agent channel accepts the strict extension
`{"op":"apply_resize","runner_args":"..."}` after `attach_tenant`. Arguments
are limited to 16 KiB and pass the normal tenant argument parser; unknown fields
are refused. The helper must keep its cold-wake admission barrier closed between
attachment and activation's reply: attachment alone starts the old-argument
runner and is not authorization to send a job. Activation refuses `resize_busy`
if any work remains, closes runner admission with a complete drain, stops the old
runner, checks the mounted XFS against `/dev/vdb`, and grows only a larger device.
It then writes arguments with file fsync, atomic rename, directory fsync and
syncfs, restarts the runner, and waits for its authenticated status/health reply.
Any failed step refuses the operation; the caller must keep the staged arguments
and retry on a fresh cold wake. Re-delivery rewrites the same argument bytes and
does not grow matching geometry. The shared job protocol is unchanged.

The source release is never changed: an exclusive temporary image version
hard-links its read-only kernel/rootfs and copies the manifest with boot-only
extra arguments. Provisioning builds a new size base from `manifest.boot_args`.
The systemd debug generator receives `motor-qualification.service` as a binary
kernel credential and wants it only on this boot. Its kernel-command-line
condition is a second opt-in fence. A boot-only drop-in sends the existing image
check directly to ttyS0, avoiding journald prefixes on exact evidence lines.
The unit waits for real tenant attachment, runs `hosted_image_check.py --tenant`,
the name-service probe below, and the delivered
`isolation::tests::hosted_job_cannot_open_vsock --ignored --exact` test as
root, and later checks for failed guest units. There is no
privileged guest-exec RPC, SSH service, or normal-boot qualification unit.

The name-service probe checks what once stalled tenant runner startup. Attach
keeps the VM-global resolver empty and maps the tenant hostname in `/etc/hosts`.
The daemon and its trusted snapshot helpers bind the same private, root-only
`/run/motor-tenant-daemon/resolv.conf` read-only over `/etc/resolv.conf`; the
backing file contains the attachment's resolver addresses and
`options timeout:2 attempts:1`. No `RESOLV_CONF` override is used. Helpers are
separate systemd services, not children that inherit the daemon's private mount
namespace, so their resolver bind must be explicit
(`tenant_snapshot_helpers_bind_daemon_dns_without_hiding_etc`). They retain the
VM's network namespace and `/etc/nsswitch.conf`; `net` jobs get separate network
namespaces and resolver bindings.
The root namespace has a default route after attach, but DNS reachability during
early startup must not be assumed. With `nameserver 1.1.1.1` temporarily in the
global resolver, `getent ahosts <hostname>` and `sudo -n true` must each
finish within 3 s. The runner unit's start to the attach completion marker must
take under 10 s. The step passes only on the exact `NAME_SERVICE` line.

The host-only port-54 `diagnostics` operation reports daemon routes, resolver
addresses/mode and namespace IDs, plus a bounded lookup of `index.crates.io` from
the daemon's mount and network namespaces. `{"op":"diagnostics","https":true}`
also makes a bounded read of that registry's fixed HTTPS config URL. No caller
can supply a hostname, URL or executable. Default jobs retain their empty network
namespace and read-only shared Cargo registry/Git mounts. Tenant preparation
allows 120 seconds for online locked fetch, then 30 seconds for offline cache
fallback; missing locked sources refuse before Started with provisioning guidance.

The tool-tree probe then checks that runner startup copied nothing from
`/usr/local/lib/motor/rustup` or `/usr/local/lib/motor/cargo` into the overlay
upper directory, and that every entry there is root-owned, world-readable and
without an extended ACL (the image build verifies the same, recorded as
`world_readable_tool_trees` in `tools.json`). It prints
`GUEST_TOOL_TREE_MEASUREMENT` evidence: entry and byte counts, stat-walk time,
overlay metacopy setting, and a timed `setfacl` over a 32 MiB sample of the
toolchain, extrapolated by bytes to the whole tree. The step passes only on the
exact `TOOL_TREES` line.

The root-path step checks that everything root executes from is root-owned
with no group or other write: every entry under `/usr/local`, `/usr/bin`,
`/usr/sbin`, `/bin` and `/sbin`, plus each directory above them. Symlinks must
be root-owned too. The list follows the PATH of the guest units. The harness
stages `guest/root_paths.py` from the operator checkout beside `run.py`, so a
booted image that drifted from the current rule fails. The image build enforces
the same rule last, recorded as `root_owned_paths` in `tools.json`. The guest
prints `GUEST_ROOT_PATHS` evidence (entry count and the first offending paths),
and the step passes only on the exact `ROOT_PATHS` line. Release archives are
extracted with `--no-same-owner --no-same-permissions` under umask 022, because
Node's tarball records uid 1001 and tar run as root would otherwise hand
`/usr/local/bin` to that uid.

The harness copies `motor-vm-helper`, `motor-host` and `motor-fake-broker` into
a fresh root-owned mode-0755 bin directory before invoking them. Its helper config is root-owned mode 0600.
The legacy job-slot configuration still requires a loader even though this lane
uses only persistent tenant operations; a regular mode 0755 copy of the system
`true` program satisfies that startup input check without pointing at a system
symlink. No legacy job VM is exercised or claimed. Firecracker/jailer remain the
fixed, root-managed 1.17.0 installation. Startup reads the helper's unit state
and journal, then sends the read-only `adopt` RPC (which observes the tenant
without waking it) to prove the daemon is accepting requests, rather than merely
waiting for a socket pathname. If an input fails the helper's root-owner,
non-writable and no-symlink checks, the early-exit diagnostic names that input
or ancestor with its uid/gid, mode and symlink status.

The harness starts the real `motor-vm-helper` in a delegated transient unit and
the real TLS `motor-host` admission server. A separate `motor-lease` fake broker
issues a requester-bound lease for each job. The fake broker's seed is never
passed to the host. TLS SPKI pinning, the challenge signature, the runner Hello,
Git bundle upload, output frames, nonzero exit 23, second-job target path/inode
and sentinel reuse are all checked. While waiting for any reply the harness
reads past runner heartbeats, which can precede even the hello reply, within a
600 s per-job deadline. The helper performs suspend/restore with
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
paths; never recursively delete them while mounts or VMMs remain. Before removal,
the harness archives only the host journal/outbox subtree (not signer seeds,
TLS keys or broker credentials), both before stopping the host and after it
stops; the full helper journal is captured after stop as well. Already
unloaded units and missing owned files are treated as removed; access-denied,
busy-mount and other real cleanup failures still fail the lane. Failed host
units affect the teardown verdict only if this run created their exact names or
their control-group path contains the exact tenant-id component. Unrelated new
fleet failures are printed/archived, never reset or cleared. SIGINT and
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
handshake completes, for at most 60 seconds. Host guest-control diagnostics name
the tenant operation, request/reply stage, blocking-fd status and elapsed read
budget; a blocking attach read deadline is described as initialization waiting
for runner readiness, not bare EAGAIN. The host's fresh-attach read deadline is
`motor_protocol::TENANT_ATTACH_READ_DEADLINE`, derived from the guest's
`TENANT_RUNNER_READINESS_WAIT` plus a setup margin, so the guest's own
"timed out waiting for runner readiness" error arrives before the host gives up.
The boot-only qualification payload also
prints bounded runner-unit journal/MainPID/socket-peer snapshots during startup,
and forwards the transient runner's own output to the console. These are
diagnostics, not a substitute for the existing PID-bound readiness check.
Firecracker can close such an early
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
