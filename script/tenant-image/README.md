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
do not run them through the unprivileged remote-job lane. The host needs
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
  /srv/build/vm/tenant-images/EXAMPLE_VERSION
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
`/srv/build/vm/tenant-images/<version>/`, where `<version>` is the chosen release name:

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
