#!/usr/bin/env python3
"""Refresh the toolchain pins from the machine that supplies the runner tools."""

import argparse
import datetime
import json
from pathlib import Path
import re
import subprocess


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def capture():
    toolchains = {}
    for line in output("rustup", "toolchain", "list").splitlines():
        name = line.split()[0]
        version = output("rustc", f"+{name}", "--version").split()[1]
        if not re.fullmatch(r"\d+\.\d+\.\d+", version):
            raise ValueError(f"only release Rust toolchains are supported: {name}")
        targets = output("rustup", "target", "list", "--installed", "--toolchain", name)
        toolchains.setdefault(version, set()).update(targets.splitlines())
    return {
        "captured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "rust_default": output("rustc", "--version").split()[1],
        "rust_toolchains": {key: sorted(value) for key, value in sorted(toolchains.items())},
        "node": output("node", "--version").removeprefix("v"),
        "npm": output("npm", "--version"),
        "bun": output("bun", "--version"),
        "pnpm": output("pnpm", "--version"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("versions.txt"))
    args = parser.parse_args()
    versions = json.loads(args.output.read_text())
    versions.update(capture())
    if versions["pnpm"] != "11.1.2":
        raise ValueError("the image contract pins pnpm 11.1.2; review before changing it")
    args.output.write_text(json.dumps(versions, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
