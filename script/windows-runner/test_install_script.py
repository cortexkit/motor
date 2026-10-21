#!/usr/bin/env python3
"""Static checks on the Windows installer that can't wait for a Windows host."""
import re
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent

# New-LocalUser rejects a -Description longer than 48 characters, and the
# installer only discovers that after it has created its root, so an over-long
# literal leaves a half-finished install on the host.
LOCAL_USER_DESCRIPTION_MAX = 48


class InstallScriptTests(unittest.TestCase):
    def test_local_user_descriptions_fit_windows_limit(self):
        found = 0
        for script in sorted(HERE.glob("*.ps1")):
            for match in re.finditer(r"New-LocalUser\b[^\n]*?-Description\s+'([^']*)'", script.read_text()):
                found += 1
                self.assertLessEqual(len(match.group(1)), LOCAL_USER_DESCRIPTION_MAX,
                                     f"{script.name}: {match.group(1)!r}")
        self.assertGreater(found, 0, "no New-LocalUser -Description literal found to check")

    def test_cmd_never_echoes_a_variable_straight_into_a_redirect(self):
        # In cmd, `echo %errorlevel%>file` with a one-digit value reads as a
        # handle redirect (`0>`), so the file gets "ECHO is off." instead of
        # the code. Put the redirect first: `>file echo %errorlevel%`.
        for script in sorted(HERE.glob("*.ps1")):
            for number, line in enumerate(script.read_text().splitlines(), 1):
                self.assertIsNone(re.search(r"(?i)\becho\s+%\w+%\s*>", line), f"{script.name}:{number}: {line.strip()}")


if __name__ == "__main__":
    unittest.main()
