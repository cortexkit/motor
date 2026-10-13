#!/bin/sh
# Provision sandbox identities without changing any existing account's grants.
# Startup validation refuses unsafe existing accounts instead of repairing them.
set -eu

for account in motor-job motor-cache; do
  if ! getent passwd "$account" >/dev/null; then
    sudo useradd --system --user-group --home-dir /nonexistent \
      --shell /usr/sbin/nologin "$account"
  fi
  id "$account"
  getent passwd "$account"
  sudo passwd -S "$account"
done
