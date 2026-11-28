#!/bin/bash
# Operator probe: can a headless macOS job use Metal, and what does adding a
# paravirtual graphics device cost? Run it on the macOS VM host as the account
# that builds images (not root). It needs no installed runner:
#
#   script/macos-runner/metal-probe.sh <motor-macvm> <sealed-image> <new-work-directory>
#
# Use absolute paths. <motor-macvm> is the helper built from this checkout and
# signed with virtualization.entitlements, as for image building. The sealed
# image must have the macOS protocol v3 guest agent and Swift. It is cloned,
# never written. The work directory must not exist yet and must be outside the
# image and any runner state root. The probe leaves its files there.
#
# The helper boots two VMs in sequence from fresh APFS clones through the same
# job helper the runner starts. Both use the normal job CPU, RAM, shares and
# agent, with no network or window and no guest login. Only the first has a
# graphics device (one 1920x1080 display). In each VM the agent runs Swift as
# the job account. The "metal:" lines print the default device name (or "no
# Metal device") and, if present, the pixel read back after an off-screen clear.
# The headless control is expected to have no Metal device.
#
# The summary gives the time from the Virtualization framework's start call to
# the authenticated agent Hello and, after the guest command exits, the physical
# footprint in bytes of the macOS service process that runs that VM. The exit status describes whether
# both measurements and applicable render checks completed, not whether a
# Metal device was available.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <motor-macvm> <sealed-image> <new-work-directory>" >&2
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
if ! /usr/bin/codesign -d --entitlements - "$helper" 2>/dev/null |
  grep -q com.apple.security.virtualization; then
  fail "$helper lacks the virtualization entitlement; sign it with script/macos-runner/virtualization.entitlements"
fi
exec "$helper" metal-probe "$image" "$work"
