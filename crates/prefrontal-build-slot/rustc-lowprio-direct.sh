#!/bin/sh
# Worker compiles run rustc directly: a stalled cache server must not park the
# machine's compile slots. Cargo calls this as: rustc-lowprio-direct <path-to-rustc> <args...>
#
# Only a process holding one of the few build slots runs this, so the slot cap
# already bounds how much CPU worker compiles take. Do not also lower their QoS:
# under load a utility-class rustc gets a small share of a core while it holds a
# slot, and the whole compile queue stalls behind it.
#
# macOS strips every DYLD_* variable when it executes a SIP-protected binary,
# including /bin/sh. Since Rust 1.98, rust-lld and rust-objcopy need the
# toolchain lib/ directory in DYLD_FALLBACK_LIBRARY_PATH to load libLLVM.dylib;
# otherwise wasm links abort and release stripping is silently skipped.
# Recompute it from rustc's sysroot and set it with /usr/bin/env after the
# protected hop, so it survives into rustc and the tools rustc runs.
sysroot=$("$1" --print sysroot 2>/dev/null)
if [ -n "$sysroot" ] && [ -d "$sysroot/lib" ]; then
  exec /usr/bin/env \
    DYLD_FALLBACK_LIBRARY_PATH="$sysroot/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}" \
    "$@"
fi
exec "$@"
