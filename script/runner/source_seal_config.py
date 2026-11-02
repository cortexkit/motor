#!/usr/bin/env python3
"""Render explicit administrator grants for the source-seal helper."""
import argparse
import json
from pathlib import Path


def render(instances):
    states = {}
    for instance in instances:
        state, separator, value = instance.partition('=')
        roots = value.split(',')
        if not separator or not value:
            raise ValueError('expected STATE_ROOT=WORKSPACE_ROOT[,WORKSPACE_ROOT...]')
        for text in [state, *roots]:
            path = Path(text)
            if not path.is_absolute() or str(path) != text or '..' in path.parts or text == '/':
                raise ValueError('seal roots must be canonical absolute non-root paths')
            for ancestor in [path, *path.parents]:
                if ancestor.is_symlink():
                    raise ValueError('seal roots must have physical, non-symlink ancestry')
        if state in states:
            raise ValueError('duplicate state root')
        states[state] = sorted(set(roots))
    if not states:
        raise ValueError('at least one --seal-instance is required')
    return json.dumps({'states': states}, sort_keys=True, indent=2) + '\n'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seal-instance', action='append', required=True)
    args = parser.parse_args()
    print(render(args.seal_instance), end='')
