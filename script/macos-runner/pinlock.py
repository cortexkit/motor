"""Reviewed macOS pins and candidate measurements. A candidate is not a release."""

import argparse
import copy
import json
from pathlib import Path
import re

import parity

HERE = Path(__file__).resolve().parent
PENDING = "pending measurement"
AGENT_COMMIT = "27b3c6cecc2d4f1b439434d246fc41d6a6249804"


def seed(server):
    chains = {}
    for name, spec in server["rust_toolchains"].items():
        name = name.replace("x86_64-unknown-linux-gnu", parity.HOST)
        chains[name] = {
            "rustc": spec["rustc"],
            "targets": [parity.HOST],
            "components": sorted(component.replace("x86_64-unknown-linux-gnu", parity.HOST)
                                 for component in spec["components"]
                                 if not component.startswith("rust-std-")
                                 or component == "rust-std-x86_64-unknown-linux-gnu"),
            "rustc_verbose": PENDING, "sha256": PENDING,
        }
    bins = {name: copy.deepcopy(spec) for name, spec in server["cargo_binaries"].items()
            if not name.startswith("sccache")}
    for spec in bins.values():
        if not spec.get("proxy"):
            spec["sha256"] = PENDING
    installs = [copy.deepcopy(spec) for spec in server["cargo_installs"] if spec["crate"] != "sccache"]
    for spec in installs:
        if "git" in spec:
            spec["source"] = {"url": spec["git"] + "/archive/" + spec["rev"] + ".tar.gz", "sha256": PENDING}
    return {
        "schema": 1, "platform": "macos", "release_status": "pending review",
        "ipsw": {"version": "27.0.1", "build": "26A434", "file": "macos-27.0.1-26A434.ipsw",
                 "size": 26637307067,
                 "sha256": "2f016638293c3e641b8b25391a76fbc16563b3711915a5551cf8aa0f5598a5c1"},
        "observed_host": {"version": "27.0", "build": "26A425"},
        "clt": {"package_id": "com.apple.pkg.CLTools_Executables", "version": PENDING,
                "softwareupdate_label": PENDING},
        "rustup": server["rustup"], "rust_profile": server["rust_profile"],
        "rust_default": server["rust_default"].replace("x86_64-unknown-linux-gnu", parity.HOST),
        "rust_toolchains": chains, "stable_copy_from": "1.99.0-" + parity.HOST,
        "cargo_home": "/opt/motor/cargo", "rustup_home": "/opt/motor/rustup",
        "cargo_binaries": bins,
        "cargo_installs": installs,
        "sources": {
            "rustup": {"url": f"https://static.rust-lang.org/rustup/archive/{server['rustup']}/{parity.HOST}/rustup-init", "sha256": PENDING},
            "bun": {"url": f"https://github.com/oven-sh/bun/releases/download/bun-v{server['tools']['bun']['version']}/bun-darwin-aarch64.zip", "sha256": PENDING},
            "git": {"url": server["tools"]["git"]["source"]["url"], "sha256": server["tools"]["git"]["source"]["sha256"]},
            "bash": {"version": "5.2.37", "url": "https://ftp.gnu.org/gnu/bash/bash-5.2.37.tar.gz",
                     "sha256": "9599b22ecd1d5787ad7d3b7bf0c59f312b3396d1e281175dd1f8a4014da621ff"},
        },
        "tools": {
            "bun": {"path": "/opt/motor/bun/bin/bun", "version": server["tools"]["bun"]["version"], "sha256": PENDING},
            "git": {"path": "/opt/motor/bin/git", "version": server["tools"]["git"]["version"], "sha256": PENDING},
            "bash": {"path": "/opt/motor/bin/bash", "version": PENDING, "sha256": PENDING},
        },
        "agent": {"commit": AGENT_COMMIT, "sha256": PENDING},
        "prep_tools": {name: {"path": "bin/" + name, "sha256": PENDING} for name in ("cargo", "rustc", "bun", "git")},
        "job_environment_by_role": {"macos": copy.deepcopy(parity.ENVIRONMENT)},
    }


def pending(value, path=""):
    if isinstance(value, dict):
        return [item for key, child in value.items() for item in pending(child, path + "." + key)]
    if isinstance(value, list):
        return [item for index, child in enumerate(value) for item in pending(child, path + f"[{index}] ")]
    return [path] if value == PENDING else []


def require_ready(manifest):
    missing = pending(manifest)
    if missing or manifest.get("release_status") != "reviewed":
        raise ValueError("macOS pin lock is not reviewed; measure, review and commit the candidate first: " + ", ".join(missing))
    if not re.fullmatch(r"[a-f0-9]{40}", manifest["agent"]["commit"]):
        raise ValueError("agent needs a full pinned source commit")
    for source in [*manifest["sources"].values(), *(row["source"] for row in manifest["cargo_installs"] if "git" in row)]:
        if not parity.sha256(source.get("sha256")):
            raise ValueError("invalid source SHA-256")
    for row in [manifest["ipsw"], manifest["agent"], *manifest["prep_tools"].values()]:
        if not parity.sha256(row.get("sha256")):
            raise ValueError("invalid artifact SHA-256")
    for name, row in manifest["prep_tools"].items():
        if row["path"] != "bin/" + name or name not in ("cargo", "rustc", "bun", "git"):
            raise ValueError("invalid standalone prep tool path")
    if set(manifest["prep_tools"]) != {"cargo", "rustc", "bun", "git"}:
        raise ValueError("missing standalone prep tool")


def candidate(manifest, actual, sources, agent_sha256, prep_tools):
    result = copy.deepcopy(manifest)
    result["release_status"] = "pending review"
    result["rust_toolchains"] = actual["rust_toolchains"]
    result["clt"]["version"] = actual["clt"]
    for name, row in result["cargo_binaries"].items():
        if not row.get("proxy"):
            row["sha256"] = actual["cargo_binaries"][name]["sha256"]
    for name in result["tools"]:
        result["tools"][name].update(actual["tools"][name])
    for name, value in sources.items():
        result["sources"][name]["sha256"] = value
    result["agent"]["sha256"] = agent_sha256
    for name, value in prep_tools.items():
        result["prep_tools"][name]["sha256"] = value
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", action="store_true", required=True)
    args = parser.parse_args()
    print(json.dumps(seed(json.loads((HERE.parent / "runner/toolchain.json").read_text())), indent=2) + "\n", end="")
