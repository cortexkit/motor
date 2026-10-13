"""Reject unsupported local dependency graphs before running any compiler."""
import json
import pathlib
import subprocess
import sys

root = pathlib.Path.cwd().resolve()
metadata = subprocess.run(
    ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
    check=True, stdout=subprocess.PIPE, text=True,
)
for package in json.loads(metadata.stdout)["packages"]:
    if package["source"] is None:
        manifest = pathlib.Path(package["manifest_path"]).resolve()
        if not manifest.is_relative_to(root):
            sys.exit("dependency writer skips path dependencies outside the repository")
