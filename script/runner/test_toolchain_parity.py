import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("toolchain_parity", Path(__file__).with_name("toolchain-parity.py"))
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)


class CaptureTests(unittest.TestCase):
    def test_timeout_and_missing_binary_are_recorded(self):
        for error, message in ((subprocess.TimeoutExpired("tool", 30), "timeout"),
                               (FileNotFoundError("missing"), "missing")):
            with self.subTest(error=message), patch.object(parity.subprocess, "run", side_effect=error):
                self.assertEqual(parity.binary_version("/bin/tool", {}), {"version": None, "error": message})

    def test_sccache_inventory_never_runs_a_cache_process(self):
        with patch.object(parity.subprocess, "run") as run:
            for name in ("sccache", "sccache-0.18.0-stock"):
                self.assertEqual(parity.binary_version("/cargo/bin/" + name, {}),
                                 {"version": None, "error": "not-probed: sccache"})
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
