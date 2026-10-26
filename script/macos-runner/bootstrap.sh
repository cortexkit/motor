#!/bin/sh
# Run only inside the disposable image-builder guest, as its Setup Assistant admin.
set -eu
scratch=$1
label=$2
mode=$3
if [ "$label" = 'pending measurement' ] || [ -z "$label" ]; then
    echo 'An explicit softwareupdate CLT label is required for measurement.' >&2
    exit 1
fi
# softwareupdate offers the Command Line Tools only while this marker exists.
trigger=/tmp/.com.apple.dt.CommandLineTools.installondemand.in-progress
touch "$trigger"
sudo -n /usr/sbin/softwareupdate --install "$label" --verbose
rm -f "$trigger"
if [ "$mode" = measure ]; then
    sudo -n /usr/bin/python3 "$scratch/recipe/guest.py" provision --scratch "$scratch" --measure
else
    sudo -n /usr/bin/python3 "$scratch/recipe/guest.py" provision --scratch "$scratch"
fi
