"""Compute SHA-256, compare available pins; GitHub uses up to six parallel ranges."""

from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import shutil
import urllib.request

from parity import digest, sha256


def fetch(url, destination, expected, measure=False):
    destination = Path(destination)
    if not sha256(expected) and not measure:
        raise ValueError("download has no reviewed SHA-256: " + url)
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(destination.name + ".download")
    try:
        if "github.com/" in url or "githubusercontent.com/" in url:
            try:
                parallel_ranges(url, temporary)
            except RangeUnsupported:
                # Parallel ranges only speed the download up; the SHA-256 check
                # below still decides whether the bytes are accepted.
                stream_to(url, temporary)
        else:
            stream_to(url, temporary)
        actual = digest(temporary)
        if sha256(expected) and actual != expected:
            raise ValueError("download SHA-256 differs: " + url)
        temporary.replace(destination)
        return actual
    finally:
        temporary.unlink(missing_ok=True)


class RangeUnsupported(Exception):
    """The server can't serve this download in byte ranges."""


def stream_to(url, destination):
    with urllib.request.urlopen(url, timeout=120) as response, Path(destination).open("wb") as stream:
        shutil.copyfileobj(response, stream, 1024 * 1024)


def parallel_ranges(url, destination, workers=6):
    with urllib.request.urlopen(urllib.request.Request(url, method="HEAD"), timeout=60) as response:
        size = int(response.headers.get("Content-Length", "0"))
    if size <= 0:
        # Source archives are generated on demand and sent chunked, without a
        # length to split into ranges.
        raise RangeUnsupported(url)
    workers = min(workers, size)
    parts = [destination.with_name(destination.name + f".part-{index}") for index in range(workers)]

    def part(index):
        start, end = size * index // workers, size * (index + 1) // workers - 1
        request = urllib.request.Request(url, headers={"Range": f"bytes={start}-{end}", "Accept-Encoding": "identity"})
        with urllib.request.urlopen(request, timeout=120) as response, parts[index].open("wb") as stream:
            if response.status != 206 or response.headers.get("Content-Range") != f"bytes {start}-{end}/{size}":
                raise RangeUnsupported(url)
            shutil.copyfileobj(response, stream, 1024 * 1024)
        if parts[index].stat().st_size != end - start + 1:
            raise ValueError("incomplete range download")

    try:
        with ThreadPoolExecutor(max_workers=workers) as pool:
            list(pool.map(part, range(workers)))
        with destination.open("wb") as stream:
            for path in parts:
                with path.open("rb") as source:
                    shutil.copyfileobj(source, stream, 1024 * 1024)
    finally:
        for path in parts:
            path.unlink(missing_ok=True)
