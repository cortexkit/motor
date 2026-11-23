#!/bin/sh
# Install the crates.io release pinned in toolchain.json and report its hash.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sccache"]["version"])' "$here/toolchain.json")
: "${CARGO_HOME:?set CARGO_HOME to the shared Cargo home, /srv/build/cargo-home}"
cargo install --locked --version "=$version" --force --no-track --root "$CARGO_HOME" sccache
# The old patched 0.18.0 build kept a second binary for comparison. Remove it.
rm -f "$CARGO_HOME/bin/sccache-0.18.0-stock"
sha256sum "$CARGO_HOME/bin/sccache"
