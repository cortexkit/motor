# Guest supervisor

`motor-vm-guest [--port 52]` runs as root inside the image. It accepts one
guest-vsock connection per restored VM; the host sends one `Start` and receives
`Frame`s. The public Rust types in `src/lib.rs` are the shared wire schema:
big-endian u32 byte length followed by strict JSON, at most 1 MiB. Output frames
hold byte arrays (not UTF-8 strings), separately for stdout and stderr. EOF without
a clean status is an abnormal end. A VM must be stopped after this session;
there is intentionally no second-job loop.

Image prerequisites:

* fixed virtio drives `/dev/vda` rootfs (read-only), `/dev/vdb` tree,
  `/dev/vdc` target and `/dev/vdd` cargo; data drives unmounted at snapshot time;
* `/proc`, `/sys`, `/dev` and writable `/run` mounted by image init;
* uid/gid 1000, no sudo/setuid-root binaries, and only loopback networking;
* agent base environment has the image's worker HOME, PATH, RUSTUP_HOME etc.
  Request overrides of those keys are discarded;
* real pinned Cargo at `/opt/motor/bin/cargo` (not a rustup proxy), git at
  `/usr/bin/git`, and immutable `/etc/motor-vm/cargo-config.toml`.

Each `trees` entry maps a direct child of the tree image to an absolute guest
workspace/sibling path strictly below `/job`. The client must translate workspace,
sibling, cwd and manifest paths into that root. A single allowed root prevents
job trees from covering any image toolchain or configuration, including future
system paths. Traversal, symlink components and overlapping destinations are
refused. Data mounts are nosuid,nodev. The guest
target and cargo mounts are `/run/motor-vm/target` and `/run/motor-vm/cargo`.

Before fork the agent sets realtime, credits 256 bits of host entropy and reseeds
the CRNG, mounts the drives, and starts the relay if `proxy_port` is present.
The command drops supplementary groups, gid and uid, creates its own session,
and sets no_new_privs. The agent reads both pipes concurrently, records the wait
status, kills all remaining uid-1000 processes with pidfds (including escaped
sessions/nondumpable children), drains output, reaps, syncs and unmounts. Only
then does it emit `Status { clean: true }`. Any failure closes the stream without
clean status; the host must not export that VM's images.

With `proxy_port` the relay binds **127.0.0.1:3128**, initiating a new host-CID
vsock connection for every TCP connection and preserving half-closes. HTTP policy
belongs to the host proxy. Without a port, jobs have no relay and are forced
offline. With an explicitly enabled job proxy, offline mode is disabled.

## Fetch

Fetch ignores all request environment, runs only image Cargo `fetch --locked`
from `/run/motor-vm/fetch` with the requested workspace manifest, and never runs
workspace code or a rustup proxy. The manifest's directory must hold Cargo.lock.
`git_directories` maps each exact locked git source (including revision) to the
Cargo directory name supplied from the Mac client's locked git bundles, prepared
as filtered bare repositories under
`CARGO_HOME/git/db/`. Git `insteadOf` overrides route those sources to local file
repositories. The host must reject unavailable/unauthorised sources before VM
launch; the guest additionally rejects missing/extra mappings.

Before clean status, the agent traverses CARGO_HOME, rejects symlinks/special
files/unknown paths, checks every registry archive against the locked SHA-256,
requires corresponding archives for extracted registry/src, and verifies each
git checkout's HEAD and short directory revision against locked full revisions.
Registry/src and git/checkouts are retained, not deleted. The image config bytes
must remain unchanged. The only extra cache metadata accepted is Cargo 1.99.0's
exact standard `CACHEDIR.TAG` at registry/CACHEDIR.TAG and git/CACHEDIR.TAG (bytes
captured by the real-fetch tests); other contents/locations are rejected.

## No-KVM tests

The portable suite runs on macOS or Linux. On Linux:

```
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run -p motor-vm-guest
```

Two explicit opt-ins fail rather than silently skip missing prerequisites:

```
# Operator-only: root and CAP_SYS_ADMIN on a disposable trusted Linux host, not hosted CI.
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run -p motor-vm-guest --features linux-root-tests --locked
# Network access and trusted Cargo installed: real registry and local-git fetches.
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run -p motor-vm-guest --features trusted-cargo-tests --locked
```

The unit tests cover protocol bounds, mount validation, separate output streams,
wait status, relay byte transport, clean-status refusal, allowlist/checksums and
source retention. Root tests require util-linux `/usr/bin/unshare`, `/bin/sh`,
`id`, `ps`, `grep`, `setsid`, `sleep`, and mount/PID/network namespace support.
They re-exec inside private namespaces and a temporary chroot, failing on missing
prerequisites. They check uid/groups/session/no_new_privs and inability to signal
the root agent, execute the fetch command against an image-path shell fixture
while a workspace offers its own cargo, sync/unmount a real bind mount, refuse a
symlink mount component, hide an outside canary, and reap escaped job descendants.
The shell fixture tests executable selection, not Cargo's dependency behavior;
the real-fetch feature covers Cargo. Compile/list without privileges with
`cargo nextest list -p motor-vm-guest --features linux-root-tests --locked` (using
the same environment prefix above).

Clock/entropy syscalls, the ext4 mount lifecycle, guest-vsock transport, and
snapshot restore need real-VM acceptance tests on Linux with KVM and prepared
guest images. They are outside the portable and no-KVM suites; a mock relay
connector or uid test does not exercise snapshot restore.
