#!/usr/bin/env python3
"""Run the unit tests of motor's Python scripts as one suite.

The test files live beside the scripts they cover, and some names contain a
hyphen, so they can't be imported as dotted modules; each file is loaded by
path instead. CI and the server gate both run this script, so a new test file
only needs to be added to TEST_FILES.

scripts/tests/test_build_budget.py is not listed: it is the vendored
build-slot wrapper's fixture and runs through that crate's integration test
(crates/prefrontal-build-slot/tests/it/lifecycle.py).
"""
from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

TEST_FILES = [
    "script/boat-probe/test_probe.py",
    "script/boat-probe/test_runner_prerequisites.py",
    "script/boat-probe/test_runner_host.py",
    "script/boat-probe/test_resources.py",
    "script/boat-image/test_boat_image.py",
    "script/hosted-host/test_hosted_host.py",
    "script/runner/network-check-tests.py",
    "script/runner/test_check_nested_js.py",
    "script/runner/test_check_nested_cargo.py",
    "script/runner/test_deploy.py",
    "script/runner/test_inbound_policy.py",
    "script/runner/test_review_reaper.py",
    "script/runner/test_sync_lock.py",
    "script/runner/test_toolchain_parity.py",
    "script/runner/test_cargo_seed.py",
    "script/tenant-provision/test_provision.py",
    "script/tenant-image/test_tenant_image.py",
    "script/tenant-vm-spike/test_hosted_spike.py",
    "script/windows-vm/test_control.py",
    "script/windows-vm/test_provisioning.py",
    "script/windows-runner/test_cache_manifest.py",
    "script/windows-runner/test_install_script.py",
    "script/hyperv-spike/test_seed.py",
    "script/hyperv-spike/test_account_serial.py",
    "scripts/tests/test_python_version.py",
    "scripts/tests/test_safety_fences.py",
    "scripts/tests/test_test_binary_fold.py",
    "scripts/tests/test_unreachable_modules.py",
]


def load(relative: str) -> unittest.TestSuite:
    path = ROOT / relative
    name = "script_test_" + path.stem.replace("-", "_")
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {relative}")
    module = importlib.util.module_from_spec(spec)
    # Test files import their script by relative location, so run each from
    # its own directory's view of sys.path.
    sys.path.insert(0, str(path.parent))
    try:
        sys.modules[name] = module
        spec.loader.exec_module(module)
    finally:
        sys.path.remove(str(path.parent))
    return unittest.defaultTestLoader.loadTestsFromModule(module)


def main() -> int:
    suite = unittest.TestSuite(load(relative) for relative in TEST_FILES)
    if suite.countTestCases() == 0:
        print("no script tests were loaded", file=sys.stderr)
        return 1
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
