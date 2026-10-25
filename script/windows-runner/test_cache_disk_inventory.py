#!/usr/bin/env python3
"""Guard the direct CIM inventory path and its bounded topology selection."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]


class CacheDiskInventoryTests(unittest.TestCase):
    def test_hyperv_inventory_uses_direct_wmi_without_powershell(self):
        agent = (ROOT / "crates/motor-winagent/src/main.rs").read_text()
        cache_disk = agent.split("fn cache_disk(", 1)[1].split(
            '#[cfg(not(feature = "hyperv-runner"))]', 1
        )[0]
        self.assertIn("cache_format::hyperv_disk_inventory", cache_disk)
        self.assertNotIn("powershell(", cache_disk)
        self.assertNotIn("hyperv_disk_inventory.ps1", agent)

        wmi = (ROOT / "crates/motor-winagent/src/cache_format/wmi.rs").read_text()
        self.assertIn("SELECT Number, BusType, IsBoot, IsSystem FROM MSFT_Disk", wmi)
        self.assertIn("SELECT Index, SCSILogicalUnit FROM Win32_DiskDrive", wmi)
        self.assertIn('connect_namespace("ROOT\\\\CIMV2")', wmi)
        self.assertIn("join_hyperv_disk_inventory", wmi)

    def test_selection_keeps_topology_refusals_and_its_cold_start_budget(self):
        cache = (ROOT / "crates/motor-winagent/src/cache.rs").read_text()
        self.assertIn("hyperv_selection_still_refuses_three_disks_boot_cache_and_missing_lun", cache)
        self.assertIn("unexpected non-SCSI disk in Hyper-V job", cache)

        steps = (ROOT / "crates/motor-winagent/src/cache_steps.rs").read_text()
        self.assertIn("pub const HYPERV_DISK_INVENTORY_TIMEOUT: Duration = Duration::from_secs(5)", steps)
        self.assertIn("CacheStep::DiskSelection => HYPERV_DISK_INVENTORY_TIMEOUT", steps)
        self.assertIn("allow five seconds for cold storage-provider", steps)


if __name__ == "__main__":
    unittest.main()
