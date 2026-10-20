#!/usr/bin/env bash
# Run offline after install-cross.sh; the private Microsoft cache is never bundled.
set -euo pipefail
cd "$(dirname "$0")/../.."
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
mapfile -t pins < <(python3 - <<'PY'
import json
p=json.load(open('script/runner/toolchain.json'))['windows_cross']
for k in ('cargo_xwin','vs_manifest','sdk','crt','arch','variant','cache','tools'): print(p[k])
PY
)
export XWIN_VERSION=${pins[1]} XWIN_SDK_VERSION=${pins[2]} XWIN_CRT_VERSION=${pins[3]} XWIN_ARCH=${pins[4]} XWIN_VARIANT=${pins[5]}
export XWIN_CACHE_DIR="$PWD/${pins[6]}"
export PATH="$PWD/${pins[7]}/bin:$PATH"
export RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= CARGO_NET_OFFLINE=true
export RUSTFLAGS='-C target-feature=+crt-static'
export CARGO_TARGET_DIR="$PWD/target"
cargo xwin --version
python3 script/windows-runner/cache-manifest.py "$XWIN_CACHE_DIR" .cortexkit/internal-docs/investigations/ally-windows-sdk-sha256.json script/runner/toolchain.json
rustc --version
cargo --version
cargo xwin build --release --locked --offline --target x86_64-pc-windows-msvc -p motor-runner
cargo xwin build --release --locked --offline --target x86_64-pc-windows-msvc --manifest-path crates/motor-winagent/Cargo.toml --features hyperv-runner
python3 - <<'PY'
from pathlib import Path
import hashlib, json, subprocess
commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
records = []
for name in ('motor-runner','motor-winagent'):
 p=Path(f'target/x86_64-pc-windows-msvc/release/{name}.exe')
 digest=hashlib.sha256(p.read_bytes()).hexdigest()
 records.append(dict(path=str(p), bytes=p.stat().st_size, sha256=digest))
 print(f'ARTIFACT {p} bytes={p.stat().st_size} SHA256={digest}')
pins=json.loads(Path('script/runner/toolchain.json').read_text())['windows_cross']
provenance=dict(schema=1, binary_source_commit=commit, target='x86_64-pc-windows-msvc', profile='release +crt-static', rustc=subprocess.check_output(['rustc','--version'],text=True).strip(), cargo=subprocess.check_output(['cargo','--version'],text=True).strip(), sdk_cache_tree_sha256=pins['cache_tree_sha256'], package_manifest_sha256=pins['package_manifest_sha256'], artifacts=records)
Path('target/windows-release-provenance.json').write_text(json.dumps(provenance,indent=2)+'\n')
PY
