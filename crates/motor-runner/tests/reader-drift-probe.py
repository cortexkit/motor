#!/usr/bin/env python3
"""Probe how environment changes affect sccache 0.18.0 cache keys, without the shared cache.

Run from a repository checkout: python3 crates/motor-runner/tests/reader-drift-probe.py
The temporary fixture/cache is under this checkout. Each variant starts a fresh
cache server and compiles the same sources to the same output path. The other-cwd
variant changes only the working directory; other variants print their environment differences.
Credential variable values are omitted from diagnostics.
Root/systemd privileges are needed only for unit variants.
Supply MOTOR_READER_DRIFT_CARGO_HOME and MOTOR_READER_DRIFT_TARGET_DIR for the
changed-path variants, and MOTOR_READER_DRIFT_SYSTEMD_USER/GROUP for unit variants.
"""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import uuid


def checked(args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def visible(key):
    return key.startswith(("CARGO", "RUST", "SCCACHE", "TMP", "TEMP", "HOME", "PATH", "USER", "LOGNAME", "MAKE", "LANG", "LC_")) and not any(word in key for word in ("TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL"))


def main():
    root = Path.cwd().resolve()
    inputs = {name: os.environ[f"MOTOR_READER_DRIFT_{name}"]
              for name in ("CARGO_HOME", "TARGET_DIR", "SYSTEMD_USER", "SYSTEMD_GROUP")}
    if not all(inputs.values()):
        raise SystemExit("MOTOR_READER_DRIFT inputs must not be empty")
    cargo_home = Path(os.environ["CARGO_HOME"])
    sccache = cargo_home / "bin/sccache"
    checked([str(sccache), "--version"])
    sysroot = checked(["rustc", "--print", "sysroot"], capture_output=True).stdout.strip()
    rustc = Path(sysroot) / "bin/rustc"
    checked([str(rustc), "--version"])
    with tempfile.TemporaryDirectory(prefix="reader-drift-", dir=root) as temporary:
        work = Path(temporary)
        cache, target = work / "cache", work / "target"
        cache.mkdir()
        target.mkdir()
        (work / "other-cwd").mkdir()
        for i in range(24):
            (work / f"dep_{i}.rs").write_text(f"pub fn value() -> u32 {{ {i} }}\n")
        base = dict(os.environ)
        base.update(SCCACHE_DIR=str(cache), SCCACHE_CACHE_SIZE="1G",
                    SCCACHE_LOCAL_RW_MODE="READ_WRITE", SCCACHE_IDLE_TIMEOUT="0")
        for key in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "SCCACHE_SERVER_PORT",
                    "SCCACHE_SERVER_UDS", "SCCACHE_IGNORE_SERVER_IO_ERROR"]:
            base.pop(key, None)
        script = work / "compile.sh"
        lines = ["#!/bin/bash", "set -euo pipefail", "env | grep -E '^(CARGO|RUST|SCCACHE|TMP|TEMP|HOME|PATH|USER|LOGNAME|MAKEFLAGS|LANG|LC_)' | grep -Ev 'TOKEN|SECRET|PASSWORD|KEY|CREDENTIAL' | sort", "pwd", "umask",
                 f"trap '{shlex.quote(str(sccache))} --stop-server >/dev/null' EXIT"]
        for i in range(24):
            lines.append(shlex.join([str(sccache), str(rustc), "--crate-name", f"dep_{i}",
                                    "--crate-type", "rlib", "--emit=link", "--out-dir",
                                    str(target), str(work / f"dep_{i}.rs")]))
        lines.append(shlex.join([str(sccache), "--show-stats"]))
        script.write_text("\n".join(lines) + "\n")
        minimal = {k: base[k] for k in ["HOME", "PATH", "CARGO_HOME", "RUSTUP_HOME",
                                       "SCCACHE_DIR", "SCCACHE_CACHE_SIZE", "SCCACHE_IDLE_TIMEOUT"]}
        variants = [("login-fill", base, False, False),
                    ("login-reader", base, False, False),
                    ("login-other-cwd", base, False, False),
                    ("minimal-shell", minimal, False, False),
                    ("minimal-systemd", minimal, True, False),
                    ("minimal-systemd-private-tmp", minimal, True, True)]
        for key, value in [("CARGO_HOME", inputs["CARGO_HOME"]),
                           ("CARGO_TARGET_DIR", inputs["TARGET_DIR"]),
                           ("CARGO_NET_OFFLINE", "true")]:
            changed = dict(base)
            changed[key] = value
            variants.append((f"login-changed-{key}", changed, False, False))
        # A direct sccache invocation with CARGO_INCREMENTAL=1 is refused at
        # startup; Cargo normally chooses incremental flags for local crates.
        keys = sorted({"TMPDIR", "TMP", "TEMP", "CARGO_MAKEFLAGS", "MAKEFLAGS",
                       "RUSTUP_TOOLCHAIN", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"}
                      | {k for k in base if k.startswith("CARGO_")})
        for key in keys:
            removed = dict(base)
            removed.pop(key, None)
            variants.append((f"login-without-{key}", removed, False, False))
            if key in base:
                restored = dict(minimal)
                restored[key] = base[key]
                variants.append((f"minimal-with-{key}", restored, True, True))
        for name, env, unit, private_tmp in variants:
            print(f"\n=== {name} ===", flush=True)
            env = dict(env)
            env["SCCACHE_LOCAL_RW_MODE"] = "READ_WRITE" if name == "login-fill" else "READ_ONLY"
            env["SCCACHE_SERVER_UDS"] = f"\\x00motor-drift-{uuid.uuid4().hex}"
            difference = {k: [base.get(k), env.get(k)] for k in sorted(base.keys() | env.keys())
                          if base.get(k) != env.get(k) and visible(k)}
            print("environment differences: " + json.dumps(difference, sort_keys=True), flush=True)
            for artifact in target.iterdir():
                if artifact.is_dir():
                    shutil.rmtree(artifact)
                else:
                    artifact.unlink()
            if unit:
                args = ["sudo", "-n", "systemd-run", "--quiet", "--pipe", "--wait", "--collect",
                        "--service-type=exec", "--expand-environment=no",
                        f"--property=User={inputs['SYSTEMD_USER']}", f"--property=Group={inputs['SYSTEMD_GROUP']}",
                        "--property=PrivateNetwork=yes", f"--property=PrivateTmp={'yes' if private_tmp else 'no'}",
                        f"--working-directory={work}"]
                args.extend(f"--setenv={k}={v}" for k, v in sorted(env.items()))
                args.extend(["/bin/bash", str(script)])
                checked(args)
            else:
                checked(["/bin/bash", str(script)], cwd=work / "other-cwd" if name == "login-other-cwd" else work, env=env)


if __name__ == "__main__":
    main()
