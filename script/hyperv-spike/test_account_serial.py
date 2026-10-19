#!/usr/bin/env python3
"""Portable checks for completed serial byte accounting, not UART hardware tests."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("motor_serial_accounting", Path(__file__).with_name("account-serial.py"))
assert spec is not None and spec.loader is not None
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def host_events(reads, writes):
    lines = []
    for kind, values in (("read", reads), ("write", writes)):
        total = 0
        for count in values:
            total += count
            lines.append(module.PREFIX + json.dumps({"event": f"{kind}-done", "detail": {"bytes": count, "total": total}}))
    return "\n".join(lines)


def guest_events(reads, writes):
    lines = []
    for kind, values in (("read", reads), ("write", writes)):
        total = 0
        for count in values:
            total += count
            lines.append(f"0 elapsed_ms=0 io {kind} done bytes={count} total={total}")
    return "\n".join(lines)


class SerialAccountingTests(unittest.TestCase):
    def test_exact_four_kib_shortfall_not_hidden_by_zero_timeouts(self):
        host = host_events([415], [4, 142512, 4, 182])
        guest = guest_events([1, 3] + [4096] * 33 + [3434] + [0] * 20, [4, 71, 4, 60, 4, 272])
        result = module.account(host, guest)
        self.assertEqual(result["host_accepted_tx"], 142702)
        self.assertEqual(result["guest_completed_rx"], 138606)
        self.assertEqual(result["end_to_end_shortfall"], 4096)
        self.assertTrue(result["reverse_counts_match"])
        self.assertIsNone(result["last_parsed_host_frame"])

    def test_transient_error_is_preserved_even_when_the_tail_is_clear(self):
        guest = guest_events([10], [5]) + "\nCOM status errors=0x1 rxover=true queued_in=130400 queued_out=0\nCOM status errors=0x0 rxover=false queued_in=0 queued_out=0"
        result = module.account(host_events([5], [10]), guest)
        self.assertEqual(result["guest_error_flags_history"], "0x1")
        self.assertEqual(result["guest_peak_input_queue"], 130400)
        self.assertEqual(result["end_to_end_shortfall"], 0)
        self.assertIn("content and later stages", result["classification"])

    def test_inconsistent_or_duplicated_trace_fails_instead_of_faking_loss(self):
        host = host_events([5], [10])
        guest = guest_events([8], [5])
        with self.assertRaisesRegex(ValueError, "inconsistent"):
            module.account(host + "\n" + host, guest)
        with self.assertRaisesRegex(ValueError, "explicit"):
            module.account("", guest)
        with self.assertRaisesRegex(ValueError, "more RX"):
            module.account(host, guest_events([11], [5]))

    def test_last_complete_host_frame_is_distinct_from_read_bytes(self):
        guest = guest_events([10], [5]) + "\n0 elapsed_ms=0 reader frame ReceiveTree bytes=3\n0 elapsed_ms=1 reader frame TreeEnd"
        result = module.account(host_events([5], [10]), guest)
        self.assertEqual(result["last_parsed_host_frame"], "TreeEnd")


if __name__ == "__main__":
    unittest.main(verbosity=2)
