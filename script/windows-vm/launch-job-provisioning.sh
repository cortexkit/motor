#!/usr/bin/env bash
# Boot the separate disk that becomes the Windows job baseline. Run it only as a
# trusted operator's transient system unit while the standing VM is stopped.
# Verify and seal the provisioned disk before using it as a job baseline.
#
# It differs from launch.sh in three ways: it boots job-provisioning.qcow2, never
# the standing VM's windows.qcow2; the setup CDs are always attached; and the NAT
# NIC has no port forward, because the job image must never run an SSH server.
# Windows Update, toolchain downloads and the agent's release build still need
# outbound network during provisioning. Job VMs boot with no NIC at all.
set -euo pipefail
root=/srv/build/windows-vm
slot=/srv/build/motor-windows/slot.lock
disk=$root/job-provisioning.qcow2
for path in "$slot" "$disk" "$root/provision/seed.iso"; do
  if [[ ! -f "$path" ]]; then
    echo "windows-job-provisioning: missing $path" >&2
    exit 1
  fi
done
# The shared slot lock keeps this VM, the standing VM and job VMs from using
# CPUs 12-15 at the same time. --no-fork keeps QEMU as the lock's holder.
set +e
/usr/bin/flock -n -E 75 --no-fork "$slot" /usr/bin/qemu-system-x86_64 \
  -name windows-job-provisioning -machine pc,accel=kvm -cpu host \
  -smp 4,sockets=1,cores=4,threads=1 -m 16384 \
  -drive "file=$disk,if=none,id=os,format=qcow2,cache=none,aio=native,discard=unmap" \
  -device virtio-blk-pci,drive=os \
  -netdev user,id=nat,net=10.0.2.0/24,dhcpstart=10.0.2.15,ipv6=off \
  -device virtio-net-pci,netdev=nat,mac=52:54:00:22:02:16 \
  -qmp "unix:$root/qmp.sock,server=on,wait=off" \
  -monitor none -display none -vga std -serial none \
  -drive "file=$root/media/server2022.iso,media=cdrom,readonly=on,index=2" \
  -drive "file=$root/media/virtio-win-0.1.302.iso,media=cdrom,readonly=on,index=3" \
  -drive "file=$root/provision/seed.iso,media=cdrom,readonly=on,index=1" \
  -boot 'order=d,once=d,menu=off'
status=$?
set -e
if [[ $status -eq 75 ]]; then
  holder=$(/usr/bin/python3 "$root/provision/slot-holder.py" "$slot")
  echo "windows-job-provisioning: $slot is held by $holder; refusing to boot" >&2
fi
exit "$status"
