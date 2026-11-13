#!/bin/bash
# Operator probe: does a build disk formatted in one macOS job VM mount in the
# next? Run it on the macOS VM host, as the account that builds images (not
# root). It needs no installed runner:
#
#   script/macos-runner/build-disk-probe.sh <motor-macvm> <sealed-image> <new work directory>
#
# <motor-macvm> is the VM helper built from this checkout and signed with
# virtualization.entitlements, as for image building. <sealed-image> is a sealed
# image directory whose guest agent speaks macOS protocol version 2; it is only
# cloned, never written. The work directory must not exist yet; everything the
# probe creates goes there, never in a runner's state root.
#
# For each way of attaching the build disk, first as a virtio block device (the
# old way) and then as a USB mass storage device (the new way), the helper's
# build-disk-probe command boots two job VMs one after the other, each from a
# fresh APFS clone of the image, through the same job helper the runner starts:
#
# 1. In VM A the image's guest agent finds the blank build disk, formats it,
#    mounts it at the checkout's target/ and runs a command that prints the
#    mount line (with its flags), what diskutil says about the device (internal
#    or removable, encrypted or not) and writes a marker file.
# 2. After VM A has stopped, VM B gets VM A's disk, as the runner hands a
#    checkout's kept disk to its next job. Its agent mounts the existing volume,
#    or prints "the build disk could not be used (<mount error>)" and reformats.
#    The command prints the mount line and reads the marker.
#
# Each build disk is a sparse file of 128 GiB, the exact size by which the agent
# recognizes it; it allocates only what the guests write.
#
# Output lines: "stdout:"/"stderr:" are the guest command's, "helper:" the job
# helper's status reports, and "RESULT <bus>:" the verdict for each attachment.
# The script exits 0 only when VM B mounted VM A's volume with the attachment
# job VMs now use (USB). The full log is kept beside the work directory as
# <work directory>.log. The work directory is deleted afterwards unless
# MOTOR_PROBE_KEEP=1 is set.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <motor-macvm> <sealed-image> <new work directory>" >&2
  exit 2
fi
helper=$1
image=$2
work=$3
fail() { echo "refused: $*" >&2; exit 2; }

[[ "$(uname -s)" == Darwin ]] || fail "run this on the macOS VM host"
[[ $EUID -ne 0 ]] || fail "run this as the image-building account, not root"
for path in "$helper" "$image" "$work"; do
  [[ "$path" == /* ]] || fail "$path is not an absolute path"
done
[[ -x "$helper" && -f "$helper" ]] || fail "$helper is not an executable file"
[[ -d "$image" ]] || fail "$image is not a directory"
[[ ! -e "$work" && ! -e "$work.log" ]] || fail "$work or $work.log already exists; name a new directory"
parent=$(dirname "$work")
[[ -d "$parent" ]] || fail "$parent does not exist"
image_real=$(cd "$image" && pwd -P)
parent_real=$(cd "$parent" && pwd -P)
# The helper wants normalized absolute paths.
work=$parent_real/$(basename "$work")
case "$parent_real/" in
  "$image_real"/*) fail "the work directory must not be inside the image" ;;
esac
# A runner's state root holds its kept build disks in targets/ and its tool
# copies in tools/. Refuse to work anywhere below one.
directory=$parent_real
while :; do
  if [[ -d "$directory/targets" && -d "$directory/tools" ]]; then
    fail "$directory looks like a runner state root; choose a work directory outside it"
  fi
  [[ "$directory" == / ]] && break
  directory=$(dirname "$directory")
done
if ! /usr/bin/codesign -d --entitlements - "$helper" 2>/dev/null |
  grep -q com.apple.security.virtualization; then
  fail "$helper lacks the virtualization entitlement; sign it with script/macos-runner/virtualization.entitlements"
fi

# shellcheck disable=SC2329 # invoked by the EXIT trap
cleanup() {
  if [[ "${MOTOR_PROBE_KEEP:-}" != 1 && -d "$work" ]]; then
    # Clones keep the image's read-only files; the directories are the probe's own.
    chmod -R u+w "$work" 2>/dev/null || true
    rm -rf "$work"
  fi
}
trap cleanup EXIT

status=0
"$helper" build-disk-probe "$image_real" "$work" 2>&1 | tee "$work.log" || status=$?
echo
echo "summary:"
grep -E '^(RESULT|stdout: (mount|diskutil):|stderr: motor:)' "$work.log" || true
echo "full log: $work.log"
exit "$status"
