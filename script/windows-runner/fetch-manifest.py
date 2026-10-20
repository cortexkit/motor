#!/usr/bin/env python3
"""Pin Microsoft's channel/package metadata before allowing a cross cache build."""
import hashlib
import json
from pathlib import Path
import sys
import urllib.request

pins = json.loads(Path("script/runner/toolchain.json").read_text())["windows_cross"]
root = Path(sys.argv[1])
root.mkdir(parents=True, exist_ok=True)
channel = urllib.request.urlopen(f"https://aka.ms/vs/{pins['vs_manifest']}/release/channel", timeout=60).read()
metadata = json.loads(channel)
items = [v for v in metadata["channelItems"] if v["id"] == "Microsoft.VisualStudio.Manifests.VisualStudio"]
if len(items) != 1:
    raise SystemExit("FAIL ambiguous Microsoft package manifest")
payload = items[0]["payloads"][0]
package = urllib.request.urlopen(payload["url"], timeout=60).read()
digest = hashlib.sha256(package).hexdigest()
if digest != pins["package_manifest_sha256"]:
    raise SystemExit(f"FAIL Microsoft manifest drift: expected {pins['package_manifest_sha256']}, got {digest}")
data = json.loads(package)
if data["info"]["buildVersion"] != pins["vs_channel_build"]:
    raise SystemExit("FAIL VS channel build changed")
(root / "channel.json").write_bytes(channel)
(root / "manifest.vsman").write_bytes(package)
print(f"Verified VS build {pins['vs_channel_build']} manifest SHA256={digest}")
