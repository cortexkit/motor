#!/usr/bin/env python3
"""Build the guest-only runner libtest against the image's own GNU libc userland."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys

HERE = Path(__file__).resolve().parent
TEST = "isolation::tests::hosted_job_cannot_open_vsock"
GIB = 1024**3
# Preserve the qualification floor and VM allocation allowance after this build.
REQUIRED_FREE = (170 + 54 + 8) * GIB
GUEST_TARGET = Path("/qualification/target")


def command(*args, stdout=None):
    argv = [str(arg) for arg in args]
    print("RUN " + json.dumps(argv), flush=True)
    result = subprocess.run(argv, stdout=stdout if stdout is not None else subprocess.PIPE,
                            text=True, timeout=1800)
    if stdout is None:
        print(result.stdout, end="", flush=True)
    result.check_returncode()
    return result.stdout or ""


def cargo_command(root, jobs):
    return ["chroot", str(root), "/usr/bin/env", "-i",
            "HOME=/root", "LANG=C.UTF-8", "CARGO_HOME=/usr/local/lib/motor/cargo",
            "RUSTUP_HOME=/usr/local/lib/motor/rustup",
            "PATH=/usr/local/lib/motor/cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "RUSTC_WRAPPER=", "CARGO_BUILD_RUSTC_WRAPPER=",
            "CARGO_TARGET_DIR=" + str(GUEST_TARGET), "TMPDIR=/qualification/tmp",
            "cargo", "test", "--manifest-path", "/qualification/source/Cargo.toml",
            "--locked", "--offline", "-j", str(jobs), "-p", "motor-runner", "--lib",
            "--target", "x86_64-unknown-linux-gnu", "--no-run", "--message-format=json"]


def artifact_path(text, output):
    paths = [row["executable"] for line in text.splitlines()
             if (row := json.loads(line)).get("reason") == "compiler-artifact"
             and row.get("target", {}).get("name") == "motor_runner" and row.get("executable")]
    if len(paths) != 1:
        raise RuntimeError(f"expected one motor-runner library test executable, got {paths}")
    guest = Path(paths[0])
    if ".." in guest.parts or not guest.is_relative_to(GUEST_TARGET):
        raise RuntimeError(f"unexpected guest target path: {guest}")
    return output / "target" / guest.relative_to(GUEST_TARGET)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024**2), b""):
            digest.update(chunk)
    return digest.hexdigest()


class Builder:
    def __init__(self, image, source, cache, output, jobs=4, execute=command):
        self.image, self.source, self.cache, self.output = [Path(p).resolve() for p in (image, source, cache, output)]
        self.jobs, self.execute = jobs, execute
        self.root = self.output / "root"
        self.mounts = []

    def mount(self, *args):
        target = Path(args[-1])
        self.execute("mount", *args)
        self.mounts.append(target)

    def bind(self, source, target, readonly=False):
        target.mkdir(parents=True, exist_ok=True)
        self.mount("--bind", source, target)
        if readonly:
            self.execute("mount", "-o", "remount,bind,ro", target)

    def cleanup(self):
        errors = []
        for target in reversed(self.mounts):
            try:
                self.execute("umount", target)
            except Exception as error:
                errors.append(f"{target}: {error}")
        if errors:
            raise RuntimeError("FAIL guest test build cleanup; retain output for inspection: " + "; ".join(errors))
        self.mounts.clear()
        # Only delete the overlay view after every recorded mount was removed.
        # Build artifacts and provenance remain outside that view.
        for name in ("root", "lower", "upper", "overlay-work", "tmp"):
            path = self.output / name
            if path.exists():
                shutil.rmtree(path)
        print("PASS guest test build cleanup: all mounts removed", flush=True)

    def build(self):
        if not 1 <= self.jobs <= 64:
            raise ValueError("jobs must be between 1 and 64")
        if any(char in str(self.output) for char in ",:\\\n"):
            raise ValueError("output contains an overlay mount-option separator")
        for path in (self.source, self.image):
            if self.output.is_relative_to(path) or path.is_relative_to(self.output):
                raise ValueError("output must be separate from the source checkout and immutable image")
        if shutil.disk_usage("/var/lib/motor-vm").free < REQUIRED_FREE:
            raise RuntimeError("need 232 GiB free: 170 GiB floor + 54 GiB qualification + 8 GiB build allowance")
        if not (self.cache / "registry").is_dir():
            raise ValueError("--cargo-cache must contain the populated host Cargo registry")
        # The only image mount is read-only; all compiler writes go to the
        # disposable overlay or the separate target/tmp bind mounts.
        self.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        for name in ("lower", "upper", "overlay-work", "root", "target", "tmp"):
            (self.output / name).mkdir()
        manifest = json.loads((self.image / "manifest.json").read_text())
        if sha256(self.image / "rootfs") != manifest["rootfs_sha256"]:
            raise RuntimeError("image rootfs hash differs from its manifest")
        source_commit = self.execute("git", "-C", self.source, "rev-parse", "HEAD").strip()
        self.execute("git", "-C", self.source, "diff", "--exit-code", "HEAD", "--")
        executable = None
        try:
            self.mount("-o", "loop,ro", self.image / "rootfs", self.output / "lower")
            options = f"lowerdir={self.output / 'lower'},upperdir={self.output / 'upper'},workdir={self.output / 'overlay-work'}"
            self.mount("-t", "overlay", "overlay", "-o", options, self.root)
            self.bind(self.source, self.root / "qualification/source", readonly=True)
            self.bind(self.output / "target", self.root / GUEST_TARGET.relative_to("/"))
            self.bind(self.output / "tmp", self.root / "qualification/tmp")
            self.bind(self.cache / "registry", self.root / "usr/local/lib/motor/cargo/registry", readonly=True)
            if (self.cache / "git").is_dir():
                self.bind(self.cache / "git", self.root / "usr/local/lib/motor/cargo/git", readonly=True)
            self.bind(Path("/dev"), self.root / "dev")
            self.mount("-t", "proc", "proc", self.root / "proc")
            self.mount("-t", "sysfs", "-o", "ro", "sysfs", self.root / "sys")
            versions = self.execute("chroot", self.root, "/usr/bin/ldd", "--version")
            build = cargo_command(self.root, self.jobs)
            env = build[:build.index("cargo")]
            versions += self.execute(*env, "rustc", "--version")
            versions += self.execute(*env, "cargo", "--version")
            artifacts = self.output / "cargo-artifacts.jsonl"
            with artifacts.open("w") as output:
                self.execute(*build, stdout=output)
            executable = artifact_path(artifacts.read_text(), self.output)
            guest_binary = GUEST_TARGET / executable.relative_to(self.output / "target")
            listing = self.execute(*env, guest_binary, "--list", "--format", "terse")
            if f"{TEST}: test" not in listing.splitlines():
                raise RuntimeError("built test binary does not contain the required isolation test")
            shutil.copyfile(executable, self.output / "runner-tests")
            (self.output / "runner-tests").chmod(0o755)
            provenance = dict(image=str(self.image), rootfs_sha256=manifest["rootfs_sha256"],
                              source_commit=source_commit, command=build, versions=versions,
                              test_binary_sha256=sha256(self.output / "runner-tests"),
                              loader_check="required test listed inside image userland; isolation test NOT run here")
            (self.output / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
            print("PASS guest test build and image loader check: " + str(self.output / "runner-tests"), flush=True)
        finally:
            self.cleanup()
        return self.output / "runner-tests"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True, type=Path)
    parser.add_argument("--source", type=Path, default=HERE.parents[1])
    parser.add_argument("--cargo-cache", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--jobs", type=int, default=4)
    args = parser.parse_args()
    if sys.platform != "linux" or os.geteuid() != 0:
        parser.error("operator-only: requires root on the dedicated Linux host")
    os.umask(0o022)
    def interrupted(signum, _frame):
        raise RuntimeError(f"interrupted by signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    for key in list(os.environ):
        if key.startswith("GIT_CONFIG_"):
            del os.environ[key]
    try:
        Builder(args.image, args.source, args.cargo_cache, args.output, args.jobs).build()
    except (Exception, KeyboardInterrupt) as error:
        print(f"FAIL guest test preparation: {error}", file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
