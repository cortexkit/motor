#!/usr/bin/env python3
"""Every Windows runner script that a person runs must load, through its
dot-sourced files, every runner function it can reach.

PowerShell resolves a function only when the call runs, so a missing
dot-source surfaces on the Windows host midway through an install, after it
has created its root. test_scripts.ps1 loads the helpers itself before it
runs the installers, which hides such a gap; this check reads the scripts
the way PowerShell loads them.
"""
import re
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent
DOT_SOURCE = re.compile(r'^\s*\.\s+"\$(?:PSScriptRoot|Root\\bin)\\([\w.-]+\.ps1)"', re.M)
DEFINITION = re.compile(r'^\s*function\s+([A-Za-z][\w-]*)', re.M)


def code(text):
    """Script text without comments or single-quoted strings, so a name in
    either isn't taken for a call. Dot-sources use double quotes and survive."""
    text = re.sub(r'<#.*?#>', '', text, flags=re.S)
    # Comments go first: an apostrophe in one would otherwise open a string.
    text = re.sub(r'(?m)(^|\s)#.*$', r'\1', text)
    return re.sub(r"'(?:[^'\n]|'')*'", "''", text)


def scripts():
    return {p.name: code(p.read_text()) for p in HERE.glob('*.ps1') if not p.name.startswith('test_')}


def missing_calls(files):
    """Each entry script's calls to runner functions its dot-sources don't load."""
    defined_in = {}
    for name, text in files.items():
        for function in DEFINITION.findall(text):
            defined_in.setdefault(function, name)
    sourced = {child for text in files.values() for child in DOT_SOURCE.findall(text)}
    missing = []
    for entry in sorted(name for name in files if name not in sourced):
        loaded, todo = set(), [entry]
        while todo:
            current = todo.pop()
            if current in loaded or current not in files:
                continue
            loaded.add(current)
            todo.extend(DOT_SOURCE.findall(files[current]))
        text = '\n'.join(files[name] for name in sorted(loaded))
        for function, home in sorted(defined_in.items()):
            if home in loaded:
                continue
            # A variable, parameter or property with the same name ($Evidence,
            # -Evidence, .Evidence) is not a call.
            if re.search(r'(?<![\w$.-])' + re.escape(function) + r'(?![\w-])', text):
                missing.append(f'{entry} reaches {function} (defined in {home}) without loading it')
    return missing


class DotSourceTests(unittest.TestCase):
    def test_entry_scripts_load_every_runner_function_they_call(self):
        files = scripts()
        self.assertNotIn('install.ps1', {c for t in files.values() for c in DOT_SOURCE.findall(t)})
        self.assertEqual(missing_calls(files), [])

    def test_a_missing_dot_source_is_reported(self):
        files = scripts()
        files['install.ps1'] = DOT_SOURCE.sub(
            lambda m: '' if m.group(1) == 'parent-identity.ps1' else m.group(0), files['install.ps1'])
        self.assertIn(
            'install.ps1 reaches Assert-RunnerPrivateDirectory (defined in parent-identity.ps1) without loading it',
            missing_calls(files))


if __name__ == '__main__':
    unittest.main()
