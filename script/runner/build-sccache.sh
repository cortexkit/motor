#!/bin/sh
# Upstream: Mozilla sccache 0.18.0 (Apache-2.0). The patch modifies local cache
# reads so read-only mounts still serve cache hits.
# Build the build server's sccache: crates.io sccache 0.18.0 with the
# read-only hits patch (script/runner/patches/sccache-0.18.0-read-only-hits.patch).
# Stock 0.18.0 refreshes an entry's file times before every cache read, so a
# job reading the shared compile cache through a read-only mount gets a miss
# on every lookup. Run as the operator on the build server:
#   sh script/runner/build-sccache.sh
# It installs standalone patched and stock binaries in $CARGO_HOME/bin and
# prints both hashes. A fresh image needs the stock binary as well.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sccache"]["version"])' "$here/toolchain.json")
patch_file="$here/patches/sccache-$version-read-only-hits.patch"
: "${CARGO_HOME:?set CARGO_HOME to the shared Cargo home, /srv/build/cargo-home}"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/sccache-build.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

# cargo install leaves the published crate source in the registry; fetch it
# there if this host has never installed sccache.
source_dir=$(ls -d "$CARGO_HOME"/registry/src/*/sccache-$version 2>/dev/null | head -n 1 || true)
if [ -z "$source_dir" ] || [ ! -e "$CARGO_HOME/bin/sccache-$version-stock" ]; then
  cargo install --locked --version "=$version" --no-track --root "$scratch/stock" sccache
  source_dir=$(ls -d "$CARGO_HOME"/registry/src/*/sccache-$version | head -n 1)
  install -m 0755 "$scratch/stock/bin/sccache" "$CARGO_HOME/bin/sccache-$version-stock"
fi
cp -R "$source_dir" "$scratch/src"
(cd "$scratch/src" && patch -p1 < "$patch_file")
cargo build --release --locked --manifest-path "$scratch/src/Cargo.toml" \
  --target-dir "$scratch/target"

built="$scratch/target/release/sccache"
bin="$CARGO_HOME/bin"
install -m 0755 "$built" "$bin/sccache.new"
mv "$bin/sccache.new" "$bin/sccache"
sha256sum "$bin/sccache" "$bin/sccache-$version-stock" 2>/dev/null || sha256sum "$bin/sccache"
