#!/usr/bin/env python3
"""Keep Hyper-V cache discovery and inventory JSON shape regression-tested."""
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
        self.assertIn("$elements = @($items | ForEach-Object { $_ | ConvertTo-Json -Compress })", script)
        self.assertIn("'[' + ($elements -join ',') + ']'", script)
        self.assertNotIn("ConvertTo-Json -InputObject $items", script)

        agent = (ROOT / "crates/motor-winagent/src/main.rs").read_text()
        cache_disk = agent.split("fn cache_disk(", 1)[1].split(
            '#[cfg(not(feature = "hyperv-runner"))]', 1
        )[0]
        self.assertRegex(
            cache_disk,
            r'(?s)powershell\(include_str!\(\s*"../../../script/windows-runner/hyperv_disk_inventory\.ps1"\s*\)\)',
        )
        self.assertIn("parse_hyperv_disk_inventory(&inventory)", cache_disk)

    def test_runner_parser_accepts_arrays_and_rejects_powershell_51_wrapper(self):
        cache = (ROOT / "crates/motor-winagent/src/cache.rs").read_text()
        self.assertIn("pub fn parse_hyperv_disk_inventory(json: &str)", cache)
        test = cache.split("fn hyperv_inventory_parser_requires_a_json_array()", 1)[1].split(
            "#[test]", 1
        )[0]
        self.assertIn('parse_hyperv_disk_inventory("[]")', test)
        self.assertIn('format!("[{os}]")', test)
        self.assertIn("value", test)

    def test_windows_fixture_checks_all_array_sizes_without_convertfrom_json(self):
        fixture = (ROOT / "script/windows-runner/test_scripts.ps1").read_text()
        inventory_fixture = fixture.split("$global:diskInventoryGetDiskCalls = 0", 1)[1].split(
            r'. "$PSScriptRoot\parent-identity.ps1"', 1
        )[0]
        self.assertIn("$global:diskInventoryGetDiskCalls++", inventory_fixture)
        self.assertIn("$global:diskInventoryPerDiskQuery", inventory_fixture)
        self.assertIn("JavaScriptSerializer", inventory_fixture)
        self.assertIn("$inventoryRoot = ,($serializer.DeserializeObject($inventoryJson))", inventory_fixture)
        self.assertIn("$inventory = $inventoryRoot[0]", inventory_fixture)
        self.assertNotIn("ConvertFrom-Json", inventory_fixture)
        self.assertIn("mode='empty'; count=0", inventory_fixture)
        self.assertIn("mode='one'; count=1", inventory_fixture)
        self.assertIn("mode='many'; count=2", inventory_fixture)
        self.assertIn(
            "Remove-Variable -Name diskInventoryGetDiskCalls,diskInventoryPerDiskQuery,diskInventoryFixtureMode -Scope Global",
            inventory_fixture,
        )


if __name__ == "__main__":
    unittest.main()
