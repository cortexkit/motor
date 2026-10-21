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


if __name__ == "__main__":
    unittest.main()
