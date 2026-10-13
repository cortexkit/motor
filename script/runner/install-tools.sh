#!/usr/bin/env bash
# Install every tool pinned in tools.txt into the build server's shared Cargo
# home. Run on the server as ubuntu, from a motor checkout:
#   bash script/runner/install-tools.sh
# cargo skips a tool already installed at the pinned version, so rerunning is
# cheap. Removing a line does not uninstall the tool or remove its links.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
export CARGO_HOME=/srv/build/cargo-home RUSTUP_HOME=/home/ubuntu/.rustup TMPDIR=/srv/build/tmp
export PATH="$CARGO_HOME/bin:$PATH"
# cargo keeps one install per crate in a root, so each revision of a git tool
# gets its own root here; its binaries are then copied into CARGO_HOME/bin.
tools_root=/srv/build/tools
# A tool from a private repository has no credential here; fail instead of
# waiting on a prompt.
export GIT_TERMINAL_PROMPT=0

# Place a copy, not a symlink, at $CARGO_HOME/bin/<name>: job sandboxes see the
# toolchain bin but not necessarily $tools_root. The rename means a job that
# starts the tool during an update sees either the old or the new binary.
place() {
  install -m 0755 "$1" "$CARGO_HOME/bin/.$2.tmp"
  mv -fT "$CARGO_HOME/bin/.$2.tmp" "$CARGO_HOME/bin/$2"
}

while read -r crate spec rev flag; do
  case "$crate" in '' | '#'*) continue ;; esac
  case "$spec" in
    git=*)
      [[ "$rev" =~ ^rev=[0-9a-f]{40}$ ]] || { echo "$crate: git tools need rev=<full sha>" >&2; exit 1; }
      sha="${rev#rev=}"
      root="$tools_root/$crate-$sha"
      cargo install --locked --root "$root" --git "${spec#git=}" --rev "$sha" "$crate"
      for bin in "$root"/bin/*; do
        name="$(basename "$bin")"
        place "$bin" "$name-${sha:0:8}"
        if [ "${flag:-}" = default ]; then place "$bin" "$name"; fi
      done
      ;;
    *)
      cargo install --locked --version "=$spec" "$crate"
      ;;
  esac
done < "$here/tools.txt"
ls -l "$CARGO_HOME/bin"
