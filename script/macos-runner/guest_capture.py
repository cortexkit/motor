#!/usr/bin/env python3
"""Capture one guest execution independently, then wait for host collection."""
import json
from pathlib import Path
import subprocess
import sys
import threading
import time


def run(command, directory, shell="/opt/motor/bin/bash", timeout=120):
    directory = Path(directory)
    directory.mkdir(mode=0o700)
    started = time.monotonic()
    child = subprocess.Popen([shell, "-c", command], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    errors = []

    def tee(source, destination, name):
        try:
            with source, (directory / name).open("xb") as capture:
                while data := source.read(65536):
                    capture.write(data)
                    destination.write(data)
                    destination.flush()
        except Exception as error:
            errors.append(error)

    threads = [threading.Thread(target=tee, args=(source, destination, name)) for source, destination, name in (
        (child.stdout, sys.stdout.buffer, "guest.stdout"),
        (child.stderr, sys.stderr.buffer, "guest.stderr"))]
    for thread in threads:
        thread.start()
    status = child.wait()
    for thread in threads:
        thread.join()
    if errors:
        raise RuntimeError("guest stream capture failed") from errors[0]
    result = {"returncode": status, "elapsed_seconds": time.monotonic() - started}
    (directory / "status.tmp").write_text(json.dumps(result) + "\n")
    (directory / "guest.status.json").write_text((directory / "status.tmp").read_text())
    # Publish only after both capture files are closed. The host acknowledges
    # after copying them outside the job tree, which cleanup will delete.
    (directory / "complete").touch()
    deadline = time.monotonic() + timeout
    while not (directory / "collected").exists():
        if time.monotonic() >= deadline:
            raise RuntimeError("host did not collect independent guest evidence")
        time.sleep(0.05)
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    sys.exit(run(sys.argv[1], sys.argv[2]))
