#!/usr/bin/env bash
# Build the build server's git release from kernel.org source and put it first
# on PATH, for the Boat and tenant images. Their distributions ship older git
# (Ubuntu 24.04: 2.43, Debian trixie: 2.47) than the build server's Ubuntu
# 26.04 package (2.53), and git behaviour differs across those releases: from
# 2.46, an empty remote.<name>.pushurl or remote.<name>.url clears the URLs
# set by earlier config, where 2.43 appended the empty string as one more URL.
#
# Run as root while building an image:
#   bash script/runner/build-git.sh
# toolchain.json's tools.git entry pins the version, source URL, source
# SHA-256 and install directory. The distribution's own git package stays
# installed at /usr/bin/git for the packages that depend on it; jobs and the
# runner find this build first because /usr/local/bin precedes /usr/bin on
# their PATH. Reapplying with unchanged inputs reuses the installed build.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# Tests stage the install below a fake root; images install at /.
install_root="${GIT_INSTALL_ROOT:-}"

spec=$(python3 - "$here/toolchain.json" <<'PY'
import json, sys
git = json.load(open(sys.argv[1]))["tools"]["git"]
source = git["source"]
print("|".join((git["version"].removeprefix("git version "), source["url"], source["sha256"],
                source["install_dir"], git["path_by_role"]["boat"].rsplit("/", 1)[0])))
PY
)
IFS='|' read -r version url sha install_dir link_dir <<< "$spec"
destination="$install_root$install_dir"
links="$install_root$link_dir"
# Executables the build server's git package puts on PATH.
commands=(git git-receive-pack git-upload-pack git-upload-archive git-shell scalar)
marker="$destination/.motor-build-inputs"
inputs="$sha $(sha256sum "$here/build-git.sh" | cut -d' ' -f1)"

fail() { echo "git $version: $*" >&2; exit 1; }

scratch=$(mktemp -d "${TMPDIR:-/tmp}/git-build.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

if [ -f "$marker" ] && [ "$(cat "$marker")" = "$inputs" ] \
    && [ "$("$destination/bin/git" --version 2>/dev/null || true)" = "git version $version" ]; then
  echo "git $version already built from the pinned source"
else
  curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error --retry 3 "$url" -o "$scratch/git.tar.xz"
  printf '%s  %s\n' "$sha" "$scratch/git.tar.xz" | sha256sum --check --status || fail "source SHA-256 mismatch"
  # Root-owned files with modes masked by umask 022, as for the other
  # release archives: no other account may write where root executes from.
  umask 022
  mkdir "$scratch/src"
  tar --no-same-owner --no-same-permissions -xJf "$scratch/git.tar.xz" --strip-components=1 -C "$scratch/src"
  # The build server's Ubuntu package is built with libcurl (GnuTLS) for
  # https, expat for http push, PCRE2 for grep -P, gettext, and reads
  # /etc/gitconfig; match those. Tcl/Tk tools are not needed by jobs.
  options=(prefix="$install_dir" sysconfdir=/etc USE_LIBPCRE2=1 NO_OPENSSL=1 USE_SRV_RR=1
           NO_TCLTK=1 DEFAULT_PAGER=pager DEFAULT_EDITOR=editor PYTHON_PATH=/usr/bin/python3)
  make -C "$scratch/src" -j"${GIT_BUILD_JOBS:-$(nproc)}" "${options[@]}" all
  make -C "$scratch/src" "${options[@]}" DESTDIR="$scratch/stage" install
  rm -rf "$destination"
  install -d -m 0755 "$destination"
  cp -a --no-preserve=ownership "$scratch/stage$install_dir/." "$destination/"
  printf '%s\n' "$inputs" > "$marker"
fi

git_bin="$destination/bin/git"
[ "$("$git_bin" --version)" = "git version $version" ] || fail "built binary reports $("$git_bin" --version)"
"$git_bin" --version --build-options | grep -q '^libcurl: ' || fail "built without libcurl (https)"
[ -x "$destination/libexec/git-core/git-remote-https" ] || fail "missing git-remote-https"
[ -x "$destination/libexec/git-core/git-http-push" ] || fail "missing git-http-push (built without expat)"
printf 'motor123\n' > "$scratch/pcre.txt"
(cd "$scratch" && "$git_bin" grep --no-index -q -P 'motor\d{3}' pcre.txt) || fail "grep -P failed (built without PCRE2)"

install -d -m 0755 "$links"
for name in "${commands[@]}"; do
  [ -x "$destination/bin/$name" ] || fail "missing $name"
  rm -f "$links/$name.tmp.$$"
  ln -s "$install_dir/bin/$name" "$links/$name.tmp.$$"
  mv -Tf "$links/$name.tmp.$$" "$links/$name"
done
echo "installed git $version at $install_dir; linked from $link_dir"
