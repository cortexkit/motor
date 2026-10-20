#!/usr/bin/env python3
"""Read-only inventory of the Linux runner's job-visible toolchain."""

import argparse
import hashlib
import json
import re
import sys
import os
from pathlib import Path
import shutil
import subprocess


def output(*args, env=None):
    result = subprocess.run(args, env=env, text=True, capture_output=True, check=True, timeout=30)
    return result.stdout.strip()


def binary_version(path, env):
    # sccache may contact a long-lived daemon even for informational commands.
    # Inventory must never start or depend on a cache server.
    if Path(path).name.startswith("sccache"):
        return {"version": None, "error": "not-probed: sccache"}
    try:
        result = subprocess.run([str(path), "--version"], env=env, text=True, capture_output=True, timeout=30)
        return {"version": (result.stdout + result.stderr).strip(), "exit_code": result.returncode}
    except subprocess.TimeoutExpired:
        return {"version": None, "error": "timeout"}
    except OSError as error:
        return {"version": None, "error": str(error)}


def capture():
    cargo_home = Path(os.environ.get("CARGO_HOME", "/srv/build/cargo-home"))
    rustup_home = Path(os.environ.get("RUSTUP_HOME", "/home/ubuntu/.rustup"))
    env = dict(os.environ, CARGO_HOME=str(cargo_home), RUSTUP_HOME=str(rustup_home),
               RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1")
    env["PATH"] = str(cargo_home / "bin") + ":/home/ubuntu/.bun/bin:" + os.environ.get("PATH", os.defpath)
    rustup = str(cargo_home / "bin/rustup")
    toolchains = {}
    for line in output(rustup, "toolchain", "list", env=env).splitlines():
        name = line.split()[0]
        toolchains[name] = {
            "rustc": output(str(rustup_home / "toolchains" / name / "bin/rustc"), "--version", env=env),
            "targets": sorted(output(rustup, "target", "list", "--installed", "--toolchain", name, env=env).splitlines()),
            "components": sorted(output(rustup, "component", "list", "--installed", "--toolchain", name, env=env).splitlines()),
        }
    binaries = {}
    for path in sorted((cargo_home / "bin").iterdir()):
        if path.is_file() and os.access(path, os.X_OK) and not path.name.startswith("."):
            binaries[path.name] = {"symlink": os.readlink(path) if path.is_symlink() else None}
            if not os.path.samefile(path, cargo_home / "bin/rustup"):
                binaries[path.name].update(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                                           **binary_version(path, env))
    metadata = {}
    for root in [cargo_home, *sorted(Path("/srv/build/tools").glob("*"))]:
        path = root / ".crates2.json"
        if path.is_file():
            metadata[str(root)] = json.loads(path.read_text())
    tools = {}
    names = {"node", "npm", "bun", "pnpm", "nats-server", "wasm-opt", "esbuild",
             "x86_64-w64-mingw32-gcc", "x86_64-w64-mingw32-g++"}
    for directory in env["PATH"].split(":"):
        if directory:
            names.update(path.name for path in Path(directory).glob("gnu*") if path.is_file())
    for name in sorted(names):
        path = shutil.which(name, path=env["PATH"])
        if path:
            owner = subprocess.run(["dpkg-query", "-S", str(Path(path).resolve())], text=True, capture_output=True, timeout=30)
            tools[name] = {"path": path, "realpath": str(Path(path).resolve()),
                           **binary_version(path, env), "package": owner.stdout.strip()}
        else:
            tools[name] = None
    packages = {}
    for line in output("dpkg-query", "-W", "-f=${Package}\t${Version}\t${db:Status-Status}\\n").splitlines():
        name, version, status = line.split("\t")
        if status == "installed":
            packages[name] = version
    return {
        "schema": 1,
        "os_release": Path("/etc/os-release").read_text(),
        "rustup_version": output(rustup, "--version", env=env),
        "rust_default": output(rustup, "default", env=env).split()[0],
        "rust_profile": output(rustup, "show", "profile", env=env),
        "rust_toolchains": toolchains,
        "cargo_home": str(cargo_home), "rustup_home": str(rustup_home),
        "cargo_binaries": binaries, "cargo_metadata": metadata,
        "tools": tools, "apt_packages": packages,
        "environment": {name: env.get(name) for name in (
            "PATH", "CC", "CXX", "WASM_BINDGEN_BIN", "WASM_OPT_BIN", "ESBUILD_BIN",
            "RUSTFLAGS", "CARGO_BUILD_TARGET", "PKG_CONFIG_PATH")},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", action="store_true", required=True,
                        help="print observed state as JSON; does not install or modify tools")
    parser.parse_args()
    print(json.dumps(capture(), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
