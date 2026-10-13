# Rust UFFD loader

Build a **static** Linux binary for the slot's empty chroot, for example:

```
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo build -p motor-vm-loader --release --target x86_64-unknown-linux-musl --locked
```

The helper executes `motor-vm-loader --memory-fd N --listen-fd N`. These are
distinct inherited fds, not paths: a read-only regular snapshot-memory file and
the bound Unix UFFD listener. The loader accepts one Firecracker connection,
receives exactly one userfaultfd with SCM_RIGHTS and a bounded JSON region array,
then closes the control connection/listener. Fragmented stream JSON is supported.

The layout must completely cover the memory fd with non-overlapping contiguous
file ranges. Host address ranges must not overlap; every address, size and offset
is 2 MiB aligned, every page size is 2 MiB, arithmetic must not overflow and every
source range must fit the fd. Only missing-page events (including writes) are
accepted, not write-protect, minor, remove or balloon events. Each fault resolves
to one 2 MiB UFFDIO_COPY; a partial copy is fatal, while EEXIST is a benign race.
No memory pathname, filesystem writes or new network sockets are needed.

Tests and benchmark:

```
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run -p motor-vm-loader --locked
# Optional real Linux UFFD gate; fails if permissions/device are unavailable.
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo test -p motor-vm-loader --features linux-uffd-tests --locked
# Operator-only root gate, with a previously built static binary, never runon:
MOTOR_VM_LOADER_BIN=/absolute/path/to/motor-vm-loader env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo nextest run -p motor-vm-loader --features linux-root-tests --locked
# Portable CPU baseline: 2048 two-MiB layout/copy iterations.
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo bench -p motor-vm-loader --bench copy --locked
# Linux syscall baseline: 2048 UFFDIO_COPY/verify/discard iterations.
env -u GIT_CONFIG_COUNT -u GIT_CONFIG_KEY_0 -u GIT_CONFIG_VALUE_0 cargo bench -p motor-vm-loader --bench copy --locked -- --uffd
```

The real UFFD gate verifies all 2 MiB of copied bytes. The syscall benchmark uses
an anonymous registered destination (not hugetlb memory); it does not substitute
for the deployment's demand-fault/snapshot qualification benchmark. Layout and
ioctl-boundary tests do not require KVM or userfaultfd permissions. The sandbox
opt-in uses the actual helper seccomp filter and an empty chroot as a slot uid;
it verifies the static binary reaches the UFFD read loop using inherited fds.
`linux-root-tests` enables this gate (the older `linux-sandbox-tests` name remains
an alias for the same test). Root, a static binary and helper seccomp support are
mandatory; failures are not skipped. Compile/list it without privileges using
`cargo nextest list -p motor-vm-loader --features linux-root-tests --locked`,
with the environment prefix shown above. The independent UFFD gate additionally
requires userfaultfd support and permission.
