"""Serve an uncached crate inside the job, without relying on a public registry."""
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import threading


name = "motor-private-cache-probe"
version = "0.0.1"
archive = io.BytesIO()
with tarfile.open(fileobj=archive, mode="w:gz") as tar:
    for path, content in {
        "Cargo.toml": f'[package]\nname = "{name}"\nversion = "{version}"\n',
        "src/lib.rs": "pub fn probe() {}\n",
    }.items():
        data = content.encode()
        entry = tarfile.TarInfo(f"{name}-{version}/{path}")
        entry.size = len(data)
        entry.mode = 0o644
        tar.addfile(entry, io.BytesIO(data))
crate = archive.getvalue()
index = json.dumps({
    "name": name, "vers": version, "deps": [],
    "cksum": hashlib.sha256(crate).hexdigest(), "features": {}, "yanked": False,
}).encode() + b"\n"


class Registry(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/config.json":
            body = json.dumps({"dl": f"{base}/crates/{{crate}}/{{version}}/download"}).encode()
        elif self.path == f"/mo/to/{name}":
            body = index
        elif self.path == f"/crates/{name}/{version}/download":
            body = crate
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


home = Path(os.environ["CARGO_HOME"])
assert str(home) == "/motor-home/cargo"
assert os.environ["CARGO_NET_OFFLINE"] == "true"
assert not list(home.glob(f"registry/cache/*/{name}-{version}.crate"))
for path in ["registry/index/seed", "registry/cache/seed", "git/db/seed"]:
    canary = home / path / "canary"
    assert canary.read_text() == "shared\n"

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Registry)
base = f"http://127.0.0.1:{server.server_port}"
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    project = Path("nested-probe")
    (project / ".cargo").mkdir(parents=True)
    (project / "src").mkdir()
    (project / "src/lib.rs").write_text("")
    (project / "Cargo.toml").write_text(
        f'[package]\nname = "nested-probe"\nversion = "0.0.0"\n'
        f'[dependencies]\n{name} = "={version}"\n'
    )
    (project / ".cargo/config.toml").write_text(
        '[source.crates-io]\nreplace-with = "probe"\n'
        f'[source.probe]\nregistry = "sparse+{base}/"\n'
    )
    subprocess.run(
        ["cargo", "fetch"], cwd=project, check=True, timeout=60,
        env={**os.environ, "CARGO_NET_OFFLINE": "false"},
    )
    downloaded = list(home.glob(f"registry/cache/*/{name}-{version}.crate"))
    assert len(downloaded) == 1
    assert downloaded[0].read_bytes() == crate
    for path in ["registry/index/seed", "registry/cache/seed", "git/db/seed"]:
        (home / path / "canary").write_text("private\n")
        (home / path / "new-private-file").write_text("private\n")
    print("downloaded private crate")
finally:
    server.shutdown()
    server.server_close()
