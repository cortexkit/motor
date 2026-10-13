#!/bin/bash
# Cargo passes the compiler as the first argument. Administrative sccache
# commands are forwarded unchanged to the installed binary.
backend=$(cat "${BASH_SOURCE[0]}.backend") || exit 2
if [[ $# == 0 || "$1" == -* ]]; then
    exec "$backend" "$@"
fi
# Server readiness is checked before compiling; never retry a compiler error.
if "${BASH_SOURCE[0]}.ready"; then
    # Cargo has already translated these into per-crate incremental flags.
    # sccache rejects a literal 1 even for non-incremental dependencies; remove
    # other values too so writer and reader dependency keys still match.
    unset CARGO_INCREMENTAL CARGO_BUILD_INCREMENTAL
    exec "$backend" "$@"
fi
# A concurrent compiler may start the same server; test the endpoint again even
# if --start-server reports that its address is already in use.
"$backend" --start-server >/dev/null 2>&1 || true
if "${BASH_SOURCE[0]}.ready"; then
    unset CARGO_INCREMENTAL CARGO_BUILD_INCREMENTAL
    exec "$backend" "$@"
fi
exec "$@"
