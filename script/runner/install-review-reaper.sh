#!/usr/bin/env bash
# Install or update the review-copy reaper on the build server. Run on the
# server from a motor checkout: `bash script/runner/install-review-reaper.sh`.
# The script is installed root-owned because a root service must not execute a
# file that an unprivileged account can rewrite.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
sudo install -o root -g root -m 0755 "$here/review_reaper.py" /usr/local/libexec/motor-review-reaper.py
sudo install -o root -g root -m 0644 "$here/review-reaper.service" /etc/systemd/system/motor-review-reaper.service
sudo install -o root -g root -m 0644 "$here/review-reaper.timer" /etc/systemd/system/motor-review-reaper.timer
sudo systemctl daemon-reload
sudo systemctl enable --now motor-review-reaper.timer
systemctl list-timers motor-review-reaper.timer --no-pager
