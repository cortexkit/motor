#!/usr/bin/env bash
# Operator-only: create a throwaway systemd image separate from the production tenant image.
set -euo pipefail
out=${1:?usage: MOTOR_RUNNER=/absolute/path build-rootfs.sh OUTPUT_DIRECTORY}
[[ $(id -u) == 0 && $(uname -m) == x86_64 && $(uname -s) == Linux ]] || { echo 'need root on Linux x86_64'; exit 1; }
: "${MOTOR_RUNNER:?set MOTOR_RUNNER to the Linux motor-runner binary from this checkout}"
test -x "$MOTOR_RUNNER" || { echo "not executable: $MOTOR_RUNNER"; exit 1; }
for tool in debootstrap mkfs.ext4 mount umount chroot truncate sha256sum; do
    command -v "$tool" >/dev/null || { echo "missing rootfs prerequisite: $tool"; exit 1; }
done
mkdir -p "$out"
out=$(cd "$out" && pwd -P)
exec > >(tee "$out/rootfs-build.log") 2>&1
test ! -e "$out/rootfs.ext4" || { echo 'refusing existing rootfs.ext4'; exit 1; }
# Match the host libc used by the supplied binary, rather than silently using an older guest libc.
# shellcheck source=/dev/null
. /etc/os-release
[[ $ID == ubuntu ]] || { echo 'this throwaway image builder needs an Ubuntu operator host'; exit 1; }
suite=$VERSION_CODENAME
echo "rootfs Ubuntu suite=$suite; runner input:"
sha256sum "$MOTOR_RUNNER"
free=$(df --output=avail -B1 "$out" | tail -n 1)
(( free > 150 * 1024**3 )) || { echo 'need >150 GiB free before image build (128 GiB fleet floor plus scratch)'; exit 1; }
root="$out/rootfs-mount"
mkdir "$root"
cleanup() {
    # Unmount children first, including pseudo-filesystems left by a failed bootstrap.
    mountpoint -q "$root" && umount --recursive "$root"
    rmdir "$root" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
truncate -s 4G "$out/rootfs.ext4"
mkfs.ext4 -F "$out/rootfs.ext4"
mount -o loop "$out/rootfs.ext4" "$root"
debootstrap --arch=amd64 --variant=minbase \
    --include=systemd-sysv,dbus,python3,sudo,acl,xfsprogs,iproute2,iputils-ping,nftables,git,ca-certificates,util-linux,procps,rustc,cargo,build-essential \
    "$suite" "$root" https://archive.ubuntu.com/ubuntu
install -D -m 0755 "$MOTOR_RUNNER" "$root/usr/local/bin/motor-runner"
here=$(cd "$(dirname "$0")" && pwd -P)
install -D -m 0755 "$here/guest.py" "$root/opt/spike/guest.py"
chroot "$root" /usr/local/bin/motor-runner --version
# Runner startup probes rustc even for shell jobs; isolated launchers also need a native linker.
chroot "$root" /usr/bin/rustc --version
chroot "$root" /usr/bin/cargo --version
chroot "$root" /usr/bin/cc --version
chroot "$root" /usr/lib/systemd/systemd --version
sysver=$(chroot "$root" /usr/lib/systemd/systemd --version | head -n 1)
[[ ${sysver#systemd } =~ ^(25[7-9]|2[6-9][0-9]|[3-9][0-9][0-9]) ]] || { echo 'need systemd >=257 for runner PrivatePIDs'; exit 1; }
for account in motor-job motor-cache motor-spike; do
    chroot "$root" useradd --system --user-group --home-dir /nonexistent --shell /usr/sbin/nologin "$account"
done
echo 'motor-spike ALL=(root) NOPASSWD: ALL' > "$root/etc/sudoers.d/motor-spike"
chmod 0440 "$root/etc/sudoers.d/motor-spike"
echo tenant-vm-spike > "$root/etc/hostname"
cat > "$root/etc/hosts" <<'EOF'
127.0.0.1 localhost
127.0.1.1 tenant-vm-spike
::1 localhost ip6-localhost ip6-loopback
EOF
echo 'LANG=C.UTF-8' > "$root/etc/default/locale"
mkdir -p "$root/etc/sysctl.d"
echo 'kernel.modules_disabled=1' > "$root/etc/sysctl.d/99-spike-modules.conf"
cat > "$root/etc/fstab" <<'EOF'
/dev/vda / ext4 defaults 0 1
tmpfs /tmp tmpfs mode=1777 0 0
EOF
mkdir -p "$root/tenant" "$root/opt/spike" "$root/var/lib/motor-spike"
cat > "$root/etc/systemd/system/spike-agent.service" <<'EOF'
[Unit]
Description=Throwaway tenant VM measurement agent (vsock only)
After=local-fs.target
[Service]
ExecStart=/usr/bin/python3 /opt/spike/guest.py
Restart=on-failure
[Install]
WantedBy=multi-user.target
EOF
cat > "$root/etc/systemd/system/spike-runner.service" <<'EOF'
[Unit]
Description=Isolated host-backend runner inside the spike VM
[Service]
User=motor-spike
Group=motor-spike
Environment=HOME=/tenant/home
Environment=CARGO_HOME=/tenant/home/.cargo
Environment=RUSTUP_HOME=/tenant/home/.rustup
ExecStart=/usr/local/bin/motor-runner serve --backend host --instance spike --state-root /tenant/state --workspace-root /tenant/work --max-weight 2 --rustc-slots 2 --sccache-size-gib 5 --free-space-floor-bytes 2147483648
EOF
chroot "$root" systemctl enable spike-agent.service
chroot "$root" systemctl mask systemd-networkd.service systemd-resolved.service apt-daily.timer apt-daily-upgrade.timer serial-getty@ttyS0.service
chroot "$root" apt-get clean
sync
cleanup
trap - EXIT
sha256sum "$out/rootfs.ext4" > "$out/rootfs.sha256"
echo 'rootfs build: passed (rootfs.ext4; runner isolation is enabled by default)'
