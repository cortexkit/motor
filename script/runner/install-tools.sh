#!/usr/bin/env bash
# Install the pinned Cargo tools and versioned runtime archives from
# toolchain.json. Run on the build server as ubuntu, or as root while building
# a Boat or tenant VM image:
#   bash script/runner/install-tools.sh [build-server|boat|tenant] [runtime-only]
# cargo skips registry tools already installed at the pinned version. Removing
# a tool entry from toolchain.json does not uninstall the tool or remove its links.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
role="${1:-build-server}"
mode="${2:-all}"
case "$role" in build-server|boat|tenant) ;; *) echo "unknown install role: $role" >&2; exit 2 ;; esac
case "$mode" in all|runtime-only) ;; *) echo "unknown install mode: $mode" >&2; exit 2 ;; esac
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

if [ "$mode" = all ]; then
tools=$(python3 -c '
import json, sys
for tool in json.load(open(sys.argv[1]))["cargo_installs"]:
    if "git" in tool:
        print(tool["crate"], "git=" + tool["git"], "rev=" + tool["rev"], "default" if tool.get("default") else "")
    else:
        print(tool["crate"], tool["version"])
' "$here/toolchain.json")
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
done <<< "$tools"
  ls -l "$CARGO_HOME/bin"
fi

install_root="${TOOLCHAIN_INSTALL_ROOT:-}"
rooted() { printf '%s%s' "$install_root" "$1"; }
as_root() {
  if [ -n "$install_root" ] || [ "$EUID" -eq 0 ]; then
    "$@"
  else
    sudo -n "$@"
  fi
}

mkdir -p "$TMPDIR"
archives=$(python3 - "$here/toolchain.json" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
for name, spec in sorted(manifest["tools"].items()):
    archive = spec.get("archive")
    if archive:
        print("|".join(str(value) for value in (
            name, spec["version"], spec["path"], archive["url"], archive["sha256"],
            archive["format"], archive["install_dir"], archive["install_path"],
            archive.get("archive_path", ""), archive.get("strip_components", 0))))
PY
)
while IFS='|' read -r name version link url sha format install_dir install_path archive_path strip_components; do
  [ -n "$name" ] || continue
  # The build server's node22 is installed by its operator; images install it
  # from the pinned archive like every other runtime.
  if [ "$name" = node22 ] && [ "$role" = build-server ]; then
    node22="$(rooted "$link")"
    actual=$("$node22" --version 2>/dev/null || true)
    [ "$actual" = "$version" ] || {
      echo "build-server node22 must already be $version at $link (observed ${actual:-missing})" >&2
      exit 1
    }
    continue
  fi

  tmp=$(mktemp -d "$TMPDIR/toolchain-archive.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error --retry 3 "$url" -o "$tmp/archive"
  printf '%s  %s\n' "$sha" "$tmp/archive" | sha256sum --check --status || {
    echo "$name: archive SHA-256 mismatch" >&2
    exit 1
  }
  destination="$(rooted "$install_dir")"
  case "$format" in
    zip)
      mkdir -p "$tmp/extracted"
      unzip -p "$tmp/archive" "$archive_path" > "$tmp/binary"
      [ -s "$tmp/binary" ] || { echo "$name: archive member is empty" >&2; exit 1; }
      as_root install -d -m 0755 "$destination/$(dirname "$install_path")"
      as_root install -m 0755 "$tmp/binary" "$destination/$install_path"
      ;;
    tar.xz)
      # Release archives record their creator's owner (Node's stores uid 1001)
      # and modes, which tar and cp -a restore when run as root. Install the
      # tree as files root owns, with modes masked by a fixed 022 umask, so no
      # other account can write where root later executes from. cp -a also
      # copies the extracted directory's own mode onto the install directory.
      (umask 022 && mkdir -p "$tmp/extracted" &&
        tar --no-same-owner --no-same-permissions -xJf "$tmp/archive" \
          --strip-components="$strip_components" -C "$tmp/extracted")
      [ -x "$tmp/extracted/$install_path" ] || { echo "$name: archive binary is missing" >&2; exit 1; }
      as_root install -d -m 0755 "$destination"
      as_root cp -a --no-preserve=ownership "$tmp/extracted/." "$destination/"
      ;;
    *) echo "$name: unsupported archive format: $format" >&2; exit 1 ;;
  esac
  link_path="$(rooted "$link")"
  target="$(rooted "$install_dir/$install_path")"
  as_root install -d -m 0755 "$(dirname "$link_path")"
  as_root rm -f "$link_path.tmp.$$"
  as_root ln -s "$target" "$link_path.tmp.$$"
  as_root mv -Tf "$link_path.tmp.$$" "$link_path"
  rm -rf "$tmp"
  trap - EXIT
  echo "installed $name $version from pinned archive"
done <<< "$archives"
