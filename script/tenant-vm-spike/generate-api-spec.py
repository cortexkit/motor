#!/usr/bin/env python3
"""Regenerate the JSON projection of the unmodified official release YAML."""

import argparse
import hashlib
import json
from pathlib import Path

import yaml

SOURCE_SHA256 = "473a0a392927b407834b341bf5fbc7925524fedaf9d925e1bbbafb6a1f48c671"
ROOT = Path(__file__).with_name("fixtures")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    source = (ROOT / "firecracker_spec-v1.17.0.yaml").read_bytes()
    if hashlib.sha256(source).hexdigest() != SOURCE_SHA256:
        raise SystemExit("release YAML digest does not match the verified artifact")
    # JSON object keys are strings, including Swagger's numeric response status keys.
    document = json.loads(json.dumps(yaml.safe_load(source)))
    rendered = json.dumps(document, sort_keys=True, indent=2) + "\n"
    target = ROOT / "firecracker_spec-v1.17.0.json"
    if args.check:
        if target.read_text() != rendered:
            raise SystemExit("JSON projection is stale or differs from the release YAML")
    else:
        target.write_text(rendered)
    print(f"PyYAML {yaml.__version__}; release schema projection: 1 check passed")
    print("JSON SHA-256 " + hashlib.sha256(rendered.encode()).hexdigest())
