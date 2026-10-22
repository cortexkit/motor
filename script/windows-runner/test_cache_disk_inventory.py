#!/usr/bin/env python3
"""Keep Hyper-V cache discovery to one storage-provider inventory query."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


class CacheDiskInventoryTests(unittest.TestCase):
    def test_hyperv_inventory_queries_storage_provider_once(self):
        source = ROOT / "script/windows-runner/hyperv_disk_inventory.ps1"
        script = source.read_text()
        calls = [line for line in script.splitlines() if re.search(r"\bGet-Disk\b", line)]
        self.assertEqual(calls, ["$disks = @(Get-Disk)"])
        self.assertNotRegex(script, r"Get-Disk\s+-Number")

        agent = (ROOT / "crates/motor-winagent/src/main.rs").read_text()
        cache_disk = agent.split("fn cache_disk(", 1)[1].split("#[cfg(not(feature = \"hyperv-runner\"))]", 1)[0]
        self.assertRegex(
            cache_disk,
            r'(?s)powershell\(include_str!\(\s*"../../../script/windows-runner/hyperv_disk_inventory\.ps1"\s*\)\)',
        )

        fixture = (ROOT / "script/windows-runner/test_scripts.ps1").read_text()
        self.assertIn("$global:diskInventoryGetDiskCalls = 0", fixture)
        self.assertIn("$global:diskInventoryGetDiskCalls++", fixture)
        self.assertIn("$global:diskInventoryPerDiskQuery", fixture)
        self.assertIn("$inventory = @((& $inventoryPath) | ConvertFrom-Json)", fixture)
        self.assertIn("Remove-Variable -Name diskInventoryGetDiskCalls,diskInventoryPerDiskQuery -Scope Global", fixture)


if __name__ == "__main__":
    unittest.main()
