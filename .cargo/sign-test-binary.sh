#!/bin/sh
# Cargo passes the executable first, followed by its original arguments.
# Nextest invokes this runner for every test, so verify a given binary only once.
if [ "$1" = '--sign-only' ]; then
  bin=$2
  stamp="${bin}.codesign-stamp"
  identity=$(stat -f '%d:%i:%z:%m:%c' "$bin" 2>/dev/null) || identity=
  if [ -n "$identity" ] && [ -r "$stamp" ]; then
    IFS= read -r stamped < "$stamp" || :
    [ "$stamped" = "$identity" ] && exit 0
  fi

  if ! codesign -v "$bin" >/dev/null 2>&1; then
    if codesign -f -s - "$bin" >/dev/null 2>&1; then
      printf 'sign-test-binary: signed %s\n' "$bin" >&2
    else
      printf 'sign-test-binary: could not sign %s; running anyway\n' "$bin" >&2
      exit 0
    fi
  fi
  # Stamp the post-signature identity, not the inode/mtime from before codesign.
  if codesign -v "$bin" >/dev/null 2>&1; then
    identity=$(stat -f '%d:%i:%z:%m:%c' "$bin" 2>/dev/null) || identity=
    if [ -n "$identity" ]; then
      temp="${stamp}.$$"
      if printf '%s\n' "$identity" > "$temp"; then
        mv -f "$temp" "$stamp" || rm -f "$temp"
      fi
    fi
  fi
  exit 0
fi

bin=$1
shift
if ! command -v codesign >/dev/null 2>&1; then
  printf 'sign-test-binary: codesign unavailable; running %s anyway\n' "$bin" >&2
else
  stamp="${bin}.codesign-stamp"
  identity=$(stat -f '%d:%i:%z:%m:%c' "$bin" 2>/dev/null) || identity=
  stamped=
  if [ -n "$identity" ] && [ -r "$stamp" ]; then
    IFS= read -r stamped < "$stamp" || :
  fi
  if [ -z "$identity" ] || [ "$stamped" != "$identity" ]; then
    # Contending nextest processes must not sign the same executable at once.
    if command -v lockf >/dev/null 2>&1; then
      lockf -k -t 30 "${stamp}.lock" "$0" --sign-only "$bin" ||
        printf 'sign-test-binary: could not lock %s; running anyway\n' "$bin" >&2
    else
      "$0" --sign-only "$bin"
    fi
  fi
fi
exec "$bin" "$@"
