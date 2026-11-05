"""One Debian snapshot mirror setting shared by debootstrap and the chroot."""

import json
import os
from pathlib import Path
import sys
from urllib.parse import urlsplit


def mirror_url(pins):
    mirror = os.environ.get("MOTOR_DEBIAN_MIRROR",
                            f"https://snapshot.debian.org/archive/debian/{pins['debian_snapshot']}/")
    parsed = urlsplit(mirror)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname or
            parsed.username is not None or parsed.password is not None or
            parsed.query or parsed.fragment or any(char.isspace() for char in mirror)):
        raise ValueError("MOTOR_DEBIAN_MIRROR must be an HTTP(S) snapshot base URL")
    return mirror.rstrip("/") + "/"


if __name__ == "__main__":
    print(mirror_url(json.loads(Path(sys.argv[1]).read_text())))
