"""Version diagnostics must precede imports that need Python 3.11."""
from pathlib import Path
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = (
    "scripts/unreachable-modules.py",
    "scripts/test-scratch.py",
    "scripts/subc-daemon-env.py",
)


class PythonVersionTest(unittest.TestCase):
    def test_old_python_exits_with_upgrade_command(self):
        for script in SCRIPTS:
            for version in ((3, 9), (3, 10)):
                with self.subTest(script=script, version=version):
                    result = subprocess.run(
                        [sys.executable, "-c",
                         f"import runpy, sys; sys.version_info = {version!r}; "
                         "runpy.run_path(sys.argv[1], run_name='__main__')",
                         script],
                        cwd=ROOT, capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(
                        result.stderr,
                        "Python 3.11 or newer is required; run: "
                        f"python3.11 {script}\n",
                    )


if __name__ == "__main__":
    unittest.main()
