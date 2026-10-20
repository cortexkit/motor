#!/usr/bin/env bash
# Explicit operator-approved, task-local cross-linker provisioning. Never global.
set -euo pipefail
cd "$(dirname "$0")/../.."
if [[ ${1:-} != --accept-microsoft-terms ]]; then
  echo 'Pass --accept-microsoft-terms only after approving the Microsoft Build Tools/SDK terms; SDK files stay private.' >&2
  exit 2
fi
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
mapfile -t pins < <(python3 - <<'PY'
import json
p=json.load(open('script/runner/toolchain.json'))['windows_cross']
for k in ('cargo_xwin','vs_manifest','sdk','crt','arch','variant','cache','tools'): print(p[k])
PY
)
export XWIN_VERSION=${pins[1]} XWIN_SDK_VERSION=${pins[2]} XWIN_CRT_VERSION=${pins[3]} XWIN_ARCH=${pins[4]} XWIN_VARIANT=${pins[5]}
export XWIN_CACHE_DIR="$PWD/${pins[6]}"
tools="$PWD/${pins[7]}"
export CARGO_HOME="$PWD/target/windows-cross/cargo-home"
export CARGO_TARGET_DIR="$PWD/target/windows-cross/tool-build"
export RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= CARGO_NET_OFFLINE=false
cargo install --locked --version "=${pins[0]}" --root "$tools" cargo-xwin
export PATH="$tools/bin:$PATH"
cargo xwin --version
python3 script/windows-runner/fetch-manifest.py target/windows-cross/provenance
cargo xwin cache xwin
python3 script/windows-runner/cache-manifest.py "$XWIN_CACHE_DIR" .cortexkit/internal-docs/investigations/ally-windows-sdk-sha256.json script/runner/toolchain.json
