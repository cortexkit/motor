"""Root-published success generation shared by host, Boat and tenant policy loads.

Remove the generation before attempting any mutation. Publish only after every
load/setup check succeeds; table existence alone must never authorize a job.
"""
import argparse
import json
import os
import stat
from pathlib import Path
import uuid

READY = Path('/run/motor-net-ready')


def require_ready():
    fd = os.open(READY, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd) as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_nlink != 1 or metadata.st_mode & 0o022 or metadata.st_size > 1024:
            raise ValueError('unsafe network success generation')
        value = json.loads(source.read(1025))
    if (not isinstance(value, dict) or set(value) != {'schema', 'generation'} or type(value['schema']) is not int or value['schema'] != 1
            or not isinstance(value['generation'], str) or len(value['generation']) != 32):
        raise ValueError('invalid network success generation')
    generation = uuid.UUID(value['generation'])
    if generation.version != 4 or generation.hex != value['generation']:
        raise ValueError('invalid network success generation')
    return generation


def require_root():
    if os.geteuid() != 0:
        raise PermissionError('network readiness requires root')


def invalidate():
    require_root()
    READY.unlink(missing_ok=True)


def publish():
    require_root()
    generation = uuid.uuid4().hex
    temporary = READY.with_name(f'.motor-net-ready-{generation}')
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
        with os.fdopen(fd, 'w') as output:
            os.fchmod(output.fileno(), 0o644)
            output.write(json.dumps({'schema': 1, 'generation': generation}) + '\n')
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, READY)
    finally:
        temporary.unlink(missing_ok=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--clear', action='store_true')
    mode.add_argument('--publish', action='store_true')
    args = parser.parse_args()
    invalidate() if args.clear else publish()
