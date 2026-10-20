#!/usr/bin/env bash
# Advisory fleet snapshot audit; stale or unavailable nested crates never gate deploy.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$here/check_nested_cargo.py" "$@"
