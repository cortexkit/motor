#!/usr/bin/env bash
# Run by the dedicated system unit, never by a fleet runner.
set -euo pipefail
root=/srv/build/windows-vm
slot=/srv/build/motor-windows/slot.lock
if [[ ! -f "$slot" ]]; then
  echo "windows-build-vm: provision the shared slot lock at $slot before starting" >&2
  exit 1
fi
media=()
if [[ -f "$root/install-mode" ]]; then
  media=(
    -drive "file=$root/media/server2022.iso,media=cdrom,readonly=on,index=2"
    -drive "file=$root/media/virtio-win-0.1.302.iso,media=cdrom,readonly=on,index=3"
    -drive "file=$root/provision/seed.iso,media=cdrom,readonly=on,index=1"
    -boot 'order=d,once=d,menu=off'
  )
fi
# --no-fork keeps the lock's owning PID alive as QEMU for holder diagnostics.
set +e
/usr/bin/flock -n -E 75 --no-fork "$slot" /usr/bin/qemu-system-x86_64 \
  -name windows-build-vm -machine pc,accel=kvm -cpu host \
  -smp 4,sockets=1,cores=4,threads=1 -m 16384 \
  -drive "file=$root/windows.qcow2,if=none,id=os,format=qcow2,cache=none,aio=native,discard=unmap" \
  -device virtio-blk-pci,drive=os \
  -netdev user,id=nat,net=10.0.2.0/24,dhcpstart=10.0.2.15,ipv6=off,hostfwd=tcp:127.0.0.1:2222-10.0.2.15:22 \
  -device virtio-net-pci,netdev=nat,mac=52:54:00:22:02:15 \
  -qmp "unix:$root/qmp.sock,server=on,wait=off" \
  -monitor none -display none -vga std -serial none \
  "${media[@]}"
status=$?
set -e
if [[ $status -eq 75 ]]; then
  holder=$(/usr/bin/python3 "$root/provision/slot-holder.py" "$slot")
  echo "windows-build-vm: $slot is held by $holder; refusing to boot" >&2
fi
exit "$status"
