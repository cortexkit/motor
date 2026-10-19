#!/usr/bin/env bash
# Operator-only: bootstrap a pinned guest; never configure the operator host.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)
out=${1:?usage: build-rootfs.sh STAGING_DIRECTORY}
out=$(cd "$out" && pwd -P)
[[ $(id -u) == 0 && $(uname -s) == Linux && $(uname -m) == x86_64 ]]
for command in debootstrap mkfs.ext4 mount umount chroot truncate; do
    command -v "$command" >/dev/null
done
suite=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["debian_suite"])' "$out/versions.json")
snapshot=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["debian_snapshot"])' "$out/versions.json")
size=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["rootfs_gib"])' "$out/versions.json")
packages=$(python3 -c 'import json,sys; print(",".join(json.load(open(sys.argv[1]))["packages"]))' "$out/versions.json")
root="$out/root"
mkdir "$root" "$out/scratch"
cleanup() {
    python3 "$here/build.py" cleanup-mounts "$root"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
debootstrap --arch=amd64 --variant=minbase --include="$packages" \
    "$suite" "$root" "https://snapshot.debian.org/archive/debian/$snapshot/"
mkdir -p "$root/srv/build" "$root/proc" "$root/sys" "$root/dev"
mount --bind "$out/scratch" "$root/srv/build"
mount -t proc proc "$root/proc"
mount -t sysfs -o ro sysfs "$root/sys"
mount --rbind /dev "$root/dev"
mount --make-rprivate "$root/dev"
cp -a "$out/script" "$out/guest" "$out/versions.json" "$out/motor-runner" "$root/srv/build/"
cp "$here/install-guest.py" "$root/srv/build/install-guest.py"
chroot "$root" /usr/bin/env -i HOME=/root PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C.UTF-8 \
    RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= \
    CARGO_BUILD_JOBS="${JOBS:?}" /usr/bin/python3 /srv/build/install-guest.py
cp "$root/srv/build/packages.txt" "$out/packages.txt"
cp "$root/srv/build/tools.json" "$out/tools.json"
cleanup
trap - EXIT
truncate -s "${size}G" "$out/rootfs"
mkfs.ext4 -F -L motor-root -d "$root" "$out/rootfs"
echo 'rootfs: pinned packages and toolchains installed; chroot unmounted; ext4 lower ready'
