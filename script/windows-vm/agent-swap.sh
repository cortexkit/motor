#!/usr/bin/env bash
# Replace the job agent inside a new copy of the sealed Windows job baseline,
# offline. Run on the build server as the operator, on a candidate copy that no
# VM uses; the sealed original is never opened. It re-executes itself as root
# through sudo -n, because the NBD device locks are root-owned.
#   agent-swap.sh <candidate qcow2> <agent exe>
# Copy the sealed baseline to a writable candidate before this step; verify and
# seal the updated candidate before configuring any VM to use it.
set -euo pipefail
[ "$(id -u)" = 0 ] || exec sudo -n bash "$0" "$@"
candidate=$(realpath -- "$1")
agent=$(realpath -- "$2")
want=$(sha256sum -- "$agent" | cut -d' ' -f1)

sudo -n modprobe nbd max_part=16
# Take an NBD device nobody else has attached. Each device is guarded by an
# exclusive flock on /run/lock/motor-qualification-<device>.lock, which the
# Windows qualification tests in motor-runner take too, so two tools can't pick
# the same free device at once. A device with a pid file belongs to another
# qemu-nbd and is skipped; this script only disconnects what it connected.
device=
for candidate_dev in /sys/class/block/nbd*; do
  [ -d "$candidate_dev" ] || continue
  name=${candidate_dev##*/}
  [[ $name =~ ^nbd[0-9]+$ ]] || continue
  exec 9>"/run/lock/motor-qualification-$name.lock"
  flock -n 9 || continue
  [ ! -e "$candidate_dev/pid" ] || { exec 9>&-; continue; }
  device=/dev/$name
  break
done
[ -n "$device" ] || { echo "no free NBD device" >&2; exit 1; }

mountpoint=$(mktemp -d /srv/build/tmp/motor-agent-update.XXXXXX)
connected=0
cleanup() {
  sudo -n umount "$mountpoint" 2>/dev/null || true
  [ "$connected" = 1 ] && sudo -n qemu-nbd --disconnect "$device" >/dev/null 2>&1
  rmdir "$mountpoint" 2>/dev/null || true
}
trap cleanup EXIT

sudo -n qemu-nbd --format=qcow2 --connect="$device" "$candidate"
connected=1
sudo -n partprobe "$device" 2>/dev/null || true
# Partition nodes can appear after qemu-nbd returns, and udev's cached
# filesystem type can be empty; wait for the nodes and probe each directly.
for _ in $(seq 1 30); do
  compgen -G "${device}p*" >/dev/null && break
  sleep 1
done

# The OS partition is the NTFS volume holding both the registry hives and the
# installed agent. Recovery and EFI volumes are never written.
os_partition=
probed=
for part in "${device}"p*; do
  [ -b "$part" ] || continue
  type=$(sudo -n blkid -o value -s TYPE "$part" 2>/dev/null || true)
  probed+="$part ${type:-unknown}"$'\n'
  [ "$type" = ntfs ] || continue
  sudo -n mount -t ntfs3 -o ro,nosuid,nodev,noexec "$part" "$mountpoint" || continue
  if [ -f "$mountpoint/Windows/System32/config/SOFTWARE" ] &&
     [ -f "$mountpoint/ProgramData/Motor/motor-winagent.exe" ]; then
    os_partition=$part
  fi
  sudo -n umount "$mountpoint"
  [ -n "$os_partition" ] && break
done
[ -n "$os_partition" ] || { printf 'Windows OS NTFS volume not found; probed:\n%s' "$probed" >&2; exit 1; }
echo "os partition: $os_partition"

destination=$mountpoint/ProgramData/Motor/motor-winagent.exe
sudo -n mount -t ntfs3 -o rw,nosuid,nodev,noexec,windows_names "$os_partition" "$mountpoint"
echo "old agent: $(sudo -n sha256sum -- "$destination" | cut -d' ' -f1)"
# Overwrite the existing file in place, so it keeps its NTFS security descriptor.
sudo -n dd if="$agent" of="$destination" bs=1M conv=notrunc,fsync status=none
sudo -n truncate -s "$(stat -c %s -- "$agent")" -- "$destination"
sync
sudo -n umount "$mountpoint"

# Verify what the image holds after a full unmount, not a cached copy.
sudo -n mount -t ntfs3 -o ro,nosuid,nodev,noexec "$os_partition" "$mountpoint"
got=$(sudo -n sha256sum -- "$destination" | cut -d' ' -f1)
sudo -n umount "$mountpoint"
[ "$got" = "$want" ] || { echo "agent hash mismatch inside image: $got" >&2; exit 1; }
echo "new agent: $got"
