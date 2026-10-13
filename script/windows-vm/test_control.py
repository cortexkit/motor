"""Host-only QMP protocol checks; no VM or third-party Python modules required."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location("vm_control", Path(__file__).with_name("control.py"))
assert spec is not None and spec.loader is not None
control = importlib.util.module_from_spec(spec)
spec.loader.exec_module(control)


class Stream:
    def __init__(self, messages):
        self.messages = iter((json.dumps(item) + "\n").encode() for item in messages)
        self.written = b""

    def write(self, data):
        self.written += data

    def flush(self):
        pass

    def readline(self):
        return next(self.messages, b"")


class QmpProtocolTests(unittest.TestCase):
    def test_status_request_and_result(self):
        stream = Stream([{"id": "query-status", "return": {"running": True}}])
        self.assertEqual(control.execute(stream, "query-status"), {"running": True})
        self.assertEqual(json.loads(stream.written), {"execute": "query-status", "id": "query-status"})

    def test_async_events_do_not_replace_command_reply(self):
        stream = Stream([{"event": "RESET"}, {"id": "other", "return": {}}, {"id": "screendump", "return": {}}])
        self.assertEqual(control.execute(stream, "screendump", {"filename": "/private/screen.ppm"}), {})
        self.assertEqual(json.loads(stream.written)["arguments"], {"filename": "/private/screen.ppm"})

    def test_qmp_error_is_reported(self):
        stream = Stream([{"id": "system_powerdown", "error": {"desc": "unsupported"}}])
        with self.assertRaisesRegex(RuntimeError, "unsupported"):
            control.execute(stream, "system_powerdown")

    def test_disconnect_before_reply_is_reported(self):
        with self.assertRaisesRegex(RuntimeError, "disconnected before replying to query-status"):
            control.execute(Stream([]), "query-status")

    def test_guest_shutdown_event_is_observed(self):
        stream = Stream([{"event": "STOP"}, {"event": "SHUTDOWN", "data": {"guest": True, "reason": "guest-shutdown"}}])
        self.assertIsNone(control.wait_for_guest_shutdown(stream, Mock()))

    def test_host_shutdown_is_not_clean(self):
        stream = Stream([{"event": "SHUTDOWN", "data": {"guest": False, "reason": "host-signal"}}])
        with self.assertRaisesRegex(RuntimeError, "not initiated by the guest"):
            control.wait_for_guest_shutdown(stream, Mock())

    def test_disconnect_is_not_clean_guest_shutdown(self):
        with self.assertRaisesRegex(RuntimeError, "without a guest SHUTDOWN event"):
            control.wait_for_guest_shutdown(Stream([]), Mock())


if __name__ == "__main__":
    unittest.main(verbosity=2)
