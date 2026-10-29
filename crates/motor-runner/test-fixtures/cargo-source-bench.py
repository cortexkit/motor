"""Compare cold locked Cargo work with shared sources and fresh private extraction."""
import json
import os
from pathlib import Path
import subprocess
import time

private = os.environ["CARGO_HOME"]
reference = "/motor-home/reference-cargo"
results = {}
for name, home in [("shared-src", reference), ("private-src", private)]:
    environment = {
        **os.environ, "CARGO_HOME": home, "CARGO_NET_OFFLINE": "true",
        "CARGO_BUILD_JOBS": "2", "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
        "SCCACHE_DISABLE": "1", "CCACHE_DISABLE": "1",
    }
    started = time.monotonic()
    subprocess.run(["cargo", "fetch", "--locked", "--offline"], env=environment, check=True, timeout=240, stderr=subprocess.STDOUT)
    fetch_ms = round((time.monotonic() - started) * 1000)
    target = Path("target") / ("cold-" + name)
    assert not target.exists()
    started = time.monotonic()
    subprocess.run([
        "cargo", "check", "-p", "motor-runner", "--lib", "--locked", "--offline",
        "--target-dir", str(target),
    ], env=environment, check=True, timeout=600, stderr=subprocess.STDOUT)
    results[name] = {"fetch_ms": fetch_ms, "cold_check_ms": round((time.monotonic() - started) * 1000)}
print("CARGO_SOURCE_BENCH=" + json.dumps(results, sort_keys=True))
