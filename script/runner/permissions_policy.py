#!/usr/bin/env python3
"""Render only the two fixed native permission grants for a trusted operator."""
import pwd
import re
from pathlib import Path
import sys

HELPER = '/usr/local/libexec/motor-runner-permissions'
ROW = ('@OPERATOR@ ALL=(root) NOPASSWD: ' + HELPER + ' physical-chmod owner-rwX -- *, '
       + HELPER + ' physical-chmod private-rwX -- *')


def uid(name):
    return pwd.getpwnam(name).pw_uid


def render(template, operator, lookup=uid):
    if re.search(r'(?mi)^\s*[#@]include', template):
        raise ValueError('sudoers include directives are not permitted in this fragment')
    if not re.fullmatch(r'[a-z_][a-z0-9_-]*', operator) or operator in ('motor-job','motor-cache'):
        raise ValueError('invalid trusted operator account')
    operator_uid = lookup(operator)
    if operator_uid == 0:
        raise ValueError('name the non-root trusted service operator')
    for name in ('motor-job','motor-cache'):
        try:
            sandbox_uid = lookup(name)
        except KeyError:
            continue
        if operator_uid == sandbox_uid:
            raise ValueError('sandbox UID aliases must never receive the helper grant')
    rows = [line for line in template.splitlines() if line.strip() and not line.lstrip().startswith('#')]
    if rows != [ROW]:
        raise ValueError('sudoers template differs from the two fixed native operations')
    return template.replace('@OPERATOR@', operator)


if __name__ == '__main__':
    template, operator, destination = sys.argv[1:]
    Path(destination).write_text(render(Path(template).read_text(), operator))
