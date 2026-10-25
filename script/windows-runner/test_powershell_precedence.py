"""PowerShell parses `Test-Path X -and Y` as Test-Path receiving an `-and`
parameter, which fails at run time ("A parameter cannot be found that
matches parameter name 'and'"). The command must be parenthesised before it
is combined: `(Test-Path X) -and Y`. This only fails on the branch that
reaches it, so a static check catches it before an install does."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
# A cmdlet call written bare inside a condition, followed by a logical operator
# on the same call: `if (Test-Path ... -and`, `-or`, `-xor`.
BARE = re.compile(r"\(\s*(Test-Path|Get-Item|Get-ChildItem)\b[^()]*?\s-(and|or|xor)\b", re.IGNORECASE)


class PowerShellPrecedenceTest(unittest.TestCase):
    def test_cmdlet_calls_are_parenthesised_before_logical_operators(self):
        offenders = []
        for directory in ("script/windows-runner", "script/hyperv-spike", "script/windows-vm"):
            for path in sorted((ROOT / directory).rglob("*.ps1")):
                for number, line in enumerate(path.read_text(encoding="utf-8-sig").splitlines(), 1):
                    if BARE.search(line):
                        offenders.append(f"{path.relative_to(ROOT)}:{number}: {line.strip()}")
        self.assertEqual(offenders, [], "\n".join(offenders))

    def test_pattern_catches_the_bug_and_accepts_the_fix(self):
        self.assertTrue(BARE.search("if (Test-Path -LiteralPath $path -and $x) {"))
        self.assertFalse(BARE.search("if ((Test-Path -LiteralPath $path) -and $x) {"))


if __name__ == "__main__":
    unittest.main()
