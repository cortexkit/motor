"""Pinned PowerShell media for the Windows job-image seed."""
import hashlib
from pathlib import Path

ARCHIVE_NAME = "PowerShell-7.6.7-win-x64.zip"
ARCHIVE_SHA256 = "d2f952ebdbf24386cb8923553aa58e41a846db35a6155ea04c58142a4b16dbb7"


def verify_archive(archive):
    with Path(archive).open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != ARCHIVE_SHA256:
            raise ValueError("PowerShell archive SHA-256 mismatch")
