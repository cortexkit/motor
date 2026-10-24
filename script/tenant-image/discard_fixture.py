"""Cold/warm sparse-disk probe for the dedicated operator qualification lane.

The root guest payload owns allocation and trim. Ordinary sandboxed jobs only
release its measurement barriers; they never gain a privileged guest-exec RPC.
"""
from pathlib import Path
import subprocess
import time

GIB = 1024**3
MIN_RECLAIM = 900 * 1024**2


def without_discard(requests):
    """Negative control: change only the tenant drive's snapshotted discard bit."""
    return [(path, dict(body, discard=False) if path == "/drives/tenant" else body)
            for path, body in requests]


def allocated(path):
    return Path(path).stat().st_blocks * 512


def assert_reclaimed(before, after):
    reclaimed = before - after
    if reclaimed < MIN_RECLAIM:
        raise RuntimeError(f"discard reclaimed {reclaimed} bytes; need at least {MIN_RECLAIM} "
                           f"(before={before}, after={after})")
    return reclaimed


def wait_file(path, timeout=180):
    deadline = time.monotonic() + timeout
    while not path.is_file():
        if time.monotonic() >= deadline:
            raise RuntimeError(f"discard measurement barrier timed out: {path.name}")
        time.sleep(.1)


def guest_cycle(phase, tenant=Path("/tenant"), workspace=Path("/Users/qualification/probe"),
                run=subprocess.run, wait=wait_file, emit=print):
    if phase not in ("cold", "warm"):
        raise ValueError("discard phase must be cold or warm")
    if phase == "warm":
        wait(workspace / "discard-warm-start")
    payload = tenant / "qualification-discard.bin"
    if payload.exists():
        raise RuntimeError("discard payload already exists")
    run(["dd", "if=/dev/zero", f"of={payload}", "bs=1048576", "count=1024", "conv=fsync"],
        check=True, timeout=180, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    emit(f"DISCARD {phase} allocated", flush=True)
    wait(workspace / f"discard-{phase}-measured")
    payload.unlink()
    run(["sync", "-f", str(tenant)], check=True, timeout=30)
    run(["fstrim", "--verbose", str(tenant)], check=True, timeout=30)
    emit(f"DISCARD {phase} trimmed", flush=True)
