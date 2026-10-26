#!/usr/bin/env bash
# Prepare only the per-instance caller storage; enabling mappings is a separate rollout.
set -euo pipefail
sudo -n python3 - <<'PY'
import fcntl
import os
from pathlib import Path
import stat

source = Path('/srv/build/Users')
roots = [Path('/srv/build/callers') / instance / 'home' for instance in ('main', 'e2e')]
for path in [source, *roots]:
    for component in [*reversed(path.parents), path]:
        if component.is_symlink():
            raise SystemExit(f'caller root setup refuses symlink: {component}')
    if path.exists() and not path.is_dir():
        raise SystemExit(f'caller root setup requires a directory: {path}')
for root in roots:
    if root.exists() and any(root.iterdir()):
        raise SystemExit(f'caller root setup requires an empty directory: {root}')
owner = source.stat()
for root in roots:
    root.mkdir(parents=True, exist_ok=True)
    os.chown(root, owner.st_uid, owner.st_gid)
    os.chmod(root, stat.S_IMODE(owner.st_mode))
    if root.stat().st_dev != owner.st_dev:
        raise SystemExit(f'caller root is not on the {source} device: {root}')
    # Unnamed files keep a failed or interrupted probe from leaving an ownerless entry.
    src = os.open(source, os.O_TMPFILE | os.O_RDWR, 0o600)
    try:
        dst = os.open(root, os.O_TMPFILE | os.O_RDWR, 0o600)
        try:
            payload = b'motor caller root reflink probe\n'
            os.write(src, payload)
            fcntl.ioctl(dst, 0x40049409, src)  # FICLONE
            if os.read(dst, len(payload) + 1) != payload:
                raise SystemExit(f'caller root reflink verification failed: {root}')
        finally:
            os.close(dst)
    finally:
        os.close(src)
    print(f'caller root ready (empty): {root}')
PY
