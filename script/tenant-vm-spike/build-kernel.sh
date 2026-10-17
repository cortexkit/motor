#!/usr/bin/env bash
# Operator-only: build a small, ordinary-page Firecracker kernel from verified source.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
out=${1:?usage: build-kernel.sh OUTPUT_DIRECTORY}
mkdir -p "$out"
out=$(cd "$out" && pwd -P)
exec > >(tee "$out/kernel-build.log") 2>&1
[[ $(uname -m) == x86_64 && $(uname -s) == Linux ]] || { echo 'need Linux x86_64'; exit 1; }
compiler=${CC:-gcc-13}
command -v "$compiler" >/dev/null || { echo 'missing pinned kernel compiler: gcc-13 (13.4.0)'; exit 1; }
"$compiler" --version
[[ $("$compiler" -dumpfullversion -dumpversion) == 13.4.0 ]] || { echo 'kernel compiler must be pinned GCC 13.4.0'; exit 1; }
for tool in curl sha256sum tar make flex bison bc openssl python3; do
    command -v "$tool" >/dev/null || { echo "missing kernel build prerequisite: $tool"; exit 1; }
done
version=6.1.128
archive="$out/linux-$version.tar.xz"
curl --fail --location --proto '=https' --tlsv1.2 \
    "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$version.tar.xz" -o "$archive"
# From kernel.org's HTTPS sha256sums.asc; recorded here so the input cannot drift.
echo "874d67d3181570e69ac6b33853f0448f05fc90d4cf3e4baaadc4a9cede7c50f3  $archive" | sha256sum --check
test ! -e "$out/linux-$version" || { echo 'refusing existing kernel source directory'; exit 1; }
tar -xJf "$archive" -C "$out"
cd "$out/linux-$version"
make CC="$compiler" HOSTCC="$compiler" x86_64_defconfig
scripts/config --disable MODULES --disable DEBUG_INFO --disable DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT \
    --disable TRANSPARENT_HUGEPAGE --disable HUGETLBFS --disable HUGETLB_PAGE \
    --enable VIRTIO --enable VIRTIO_MMIO --enable VIRTIO_MMIO_CMDLINE_DEVICES \
    --enable VIRTIO_BLK --enable VIRTIO_NET --enable VIRTIO_CONSOLE \
    --enable VSOCKETS --enable VIRTIO_VSOCKETS --enable EXT4_FS --enable XFS_FS \
    --enable XFS_POSIX_ACL --enable TMPFS --enable TMPFS_POSIX_ACL \
    --enable OVERLAY_FS --enable POSIX_MQUEUE --enable BLK_DEV_INITRD \
    --enable CGROUPS --enable MEMCG --enable CGROUP_PIDS --enable CGROUP_SCHED \
    --enable CFS_BANDWIDTH --enable CPUSETS --enable CGROUP_DEVICE \
    --enable NAMESPACES --enable USER_NS --enable PID_NS --enable NET_NS \
    --enable SECCOMP --enable SECCOMP_FILTER --enable UNIX --enable INET \
    --enable BPF_SYSCALL --enable CGROUP_BPF --enable BPF_JIT \
    --disable IPV6_SIT --disable NET_IPGRE --disable NET_IPIP --disable IPV6_GRE --disable IPV6_TUNNEL \
    --enable NETFILTER --enable NF_TABLES --enable NF_TABLES_INET \
    --enable NF_CONNTRACK --enable NF_NAT --enable NFT_CT --enable NFT_NAT \
    --enable NFT_MASQ --enable VETH --enable DEVTMPFS --enable DEVTMPFS_MOUNT \
    --enable SERIAL_8250 --enable SERIAL_8250_CONSOLE --enable RANDOM_TRUST_CPU
make CC="$compiler" HOSTCC="$compiler" olddefconfig
python3 "$here/check-kernel-config.py" .config
make CC="$compiler" HOSTCC="$compiler" -j"${JOBS:-4}" vmlinux
cp vmlinux "$out/vmlinux"
cp .config "$out/kernel.config"
python3 "$here/check-kernel-config.py" "$out/kernel.config"
sha256sum "$out/vmlinux" "$out/kernel.config"
echo 'kernel build: passed (vmlinux and kernel.config)'
