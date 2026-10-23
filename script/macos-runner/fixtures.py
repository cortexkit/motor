#!/usr/bin/env python3
"""Create disposable, committed operator fixtures; never modify live runner caches."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import uuid


def command(*args, cwd):
    # These are new disposable repositories. Inherited Git index/worktree
    # overrides must not redirect a fixture commit into the operator's checkout.
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    return subprocess.check_output(args, cwd=cwd, text=True, env=env).strip()


def put(root, name, content):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def commit(root):
    command("git", "init", "-q", cwd=root)
    command("git", "add", ".", cwd=root)
    command("git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false",
            "-c", "user.name=Motor qualification", "-c", "user.email=qualification@example.invalid",
            "commit", "-qm", "Disposable macOS qualification fixture", cwd=root)
    return command("git", "rev-parse", "HEAD", cwd=root)


def create(root):
    root = Path(root).absolute()
    root.mkdir(mode=0o700)
    sibling, workspace = root / "sibling", root / "workspace"
    put(sibling, "Cargo.toml", '[package]\nname="motor-qualification-sibling"\nversion="1.0.0"\nedition="2024"\n')
    put(sibling, "src/lib.rs", 'pub fn answer() -> u32 { 42 }\n')
    put(sibling, "package.json", json.dumps({"name": "motor-qualification-sibling", "version": "1.0.0", "main": "index.js"}) + "\n")
    put(sibling, "index.js", 'module.exports = 42;\n')
    commit(sibling)
    put(workspace, "Cargo.toml", '[package]\nname="motor-macos-qualification"\nversion="1.0.0"\nedition="2024"\n[dependencies]\nmotor-qualification-sibling={path="../sibling"}\n')
    put(workspace, "src/lib.rs", '#[test] fn sibling_is_used() { assert_eq!(motor_qualification_sibling::answer(), 42); }\n')
    put(workspace, "package.json", json.dumps({"name": "motor-macos-qualification", "dependencies": {
        "motor-qualification-sibling": "file:../sibling", "@esbuild/darwin-arm64": "0.25.5"}}) + "\n")
    put(workspace, "verify.js", '''const assert = require("node:assert/strict");
assert.equal(require("motor-qualification-sibling"), 42);
const pkg = require("@esbuild/darwin-arm64/package.json");
assert.equal(pkg.version, "0.25.5");
assert.deepEqual(pkg.os, ["darwin"]); assert.deepEqual(pkg.cpu, ["arm64"]);
assert.equal(process.platform, "darwin"); assert.equal(process.arch, "arm64");
const result = Bun.spawnSync(["node_modules/@esbuild/darwin-arm64/bin/esbuild", "--version"]);
assert.equal(result.exitCode, 0); assert.equal(result.stdout.toString().trim(), "0.25.5");
console.log("sibling=42 darwin-arm64 esbuild=0.25.5");
''')
    put(workspace, ".gitignore", 'node_modules/\ntarget/\n')
    command("cargo", "generate-lockfile", "--offline", cwd=workspace)
    put(workspace, "bun.lock", (Path(__file__).parent / "fixtures/bun.lock.template").read_text())
    commit(workspace)

    dep = root / "private-source"
    put(dep, "Cargo.toml", '[package]\nname="motor-private-qualification"\nversion="1.0.0"\nedition="2024"\n')
    put(dep, "src/lib.rs", 'pub fn answer() -> u32 { 42 }\n')
    good = commit(dep)
    # This isolated cache is for the installed client module, not the runner.
    # Only the positive commit is present; the negative lock names a later one.
    name = "motor-private-qualification-" + uuid.uuid4().hex
    home = root / "client-cargo-home"
    cache = home / "git/db" / (name + "-fixture")
    cache.parent.mkdir(parents=True)
    command("git", "clone", "--bare", "--no-hardlinks", str(dep), str(cache), cwd=root)
    put(dep, "not-in-client-cache", "negative fixture commit\n")
    missing = commit(dep)
    url = "https://127.0.0.1:9/" + name
    for directory, revision in (("private-good", good), ("private-missing", missing)):
        target = root / directory
        put(target, "Cargo.toml", f'[package]\nname="motor-private-consumer"\nversion="1.0.0"\nedition="2024"\n[dependencies]\nmotor-private-qualification={{git="{url}",rev="{revision}"}}\n')
        put(target, "src/lib.rs", '#[test] fn private_dependency_is_used() { assert_eq!(motor_private_qualification::answer(), 42); }\n')
        put(target, "Cargo.lock", f'version = 4\n\n[[package]]\nname = "motor-private-consumer"\nversion = "1.0.0"\ndependencies = ["motor-private-qualification"]\n\n[[package]]\nname = "motor-private-qualification"\nversion = "1.0.0"\nsource = "git+{url}?rev={revision}#{revision}"\n')
        put(target, ".gitignore", 'target/\n')
        commit(target)
    result = {"workspace": str(workspace), "sibling": str(sibling), "private_good": str(root / "private-good"),
              "private_missing": str(root / "private-missing"), "client_cargo_home": str(home),
              "private_url": url, "bundled_commit": good, "missing_commit": missing}
    put(root, "fixtures.json", json.dumps(result, indent=2) + "\n")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="new absolute fixture directory under /Users on the operator Mac")
    args = parser.parse_args()
    print(json.dumps(create(args.root), indent=2))
