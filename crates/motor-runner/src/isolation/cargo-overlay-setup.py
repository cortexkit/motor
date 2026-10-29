"""Pin one unit's task and namespaces before entering them or changing mounts."""
import ctypes
import fcntl
import json
import os
import select
import stat
import subprocess
import sys


def refused(message):
    raise RuntimeError("cargo_overlay_target_refused: " + message)


def read_at(directory, name):
    fd = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=directory)
    with os.fdopen(fd, "r") as stream:
        return stream.read(65536)


def cgroup(directory):
    groups = [line.split(":", 2)[2] for line in read_at(directory, "cgroup").splitlines()
              if line.startswith("0::")]
    if len(groups) != 1:
        refused("target has no unique unified cgroup")
    return groups[0]


def identity(fd):
    entry = os.fstat(fd)
    return entry.st_dev, entry.st_ino


def setup(plan):
    descriptors = []
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW
    try:
        # The pidfd is opened first: a recycled numeric PID cannot make a dead
        # task's pidfd refer to its replacement. All subsequent proc reads use
        # one pinned directory, not another lookup of the numeric pathname.
        pidfd = os.pidfd_open(plan["pid"], 0)
        descriptors.append(pidfd)
        dead = select.poll()
        dead.register(pidfd, select.POLLIN)
        proc = os.open("/proc", directory_flags)
        descriptors.append(proc)
        target = os.open(str(plan["pid"]), directory_flags, dir_fd=proc)
        descriptors.append(target)

        def alive():
            if dead.poll(0):
                refused("gated task died")
            if cgroup(target) != plan["cgroup"]:
                refused("cgroup does not match the gated unit")

        if plan["cgroup"] in ("", "/"):
            refused("unit has no control group")
        alive()
        query = subprocess.run(
            ["/usr/bin/systemctl", "show", "--property=MainPID", "--value", plan["unit"]],
            capture_output=True, text=True, check=True,
        )
        if query.stdout.strip() != str(plan["pid"]):
            refused("unit no longer has the pinned main task")
        alive()
        namespaces = os.open("ns", directory_flags, dir_fd=target)
        descriptors.append(namespaces)
        handles = {}
        for name, kind in (("mnt", 0x20000), ("pid", 0x20000000), ("user", 0x10000000)):
            # Earlier proc directory components were opened no-follow. Follow
            # only these final namespace links to obtain the handles setns needs.
            fd = os.open(name, os.O_RDONLY | os.O_CLOEXEC, dir_fd=namespaces)
            descriptors.append(fd)
            if fcntl.ioctl(fd, 0xB703) != kind:
                refused("wrong namespace handle type: " + name)
            handles[name] = fd
        current_user = os.open("/proc/self/ns/user", os.O_RDONLY | os.O_CLOEXEC)
        descriptors.append(current_user)
        if identity(current_user) != identity(handles["user"]):
            refused("unexpected target user namespace")
        root = os.open("root", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC, dir_fd=target)
        descriptors.append(root)
        guard_fd = os.open("motor-cargo-overlay", directory_flags, dir_fd=root)
        descriptors.append(guard_fd)
        guard = os.fstat(guard_fd)
        if not stat.S_ISDIR(guard.st_mode) or (guard.st_dev, guard.st_ino, guard.st_uid) != tuple(plan["guard"]):
            refused("private layer guard does not belong to the gated unit")
        if stat.S_IMODE(guard.st_mode) != 0o700:
            refused("private layer guard is not owner-only")
        alive()

        # This separate trusted process enters pinned handles, never a fresh PID.
        # setns(PID) affects children; fork only after choosing that namespace.
        libc = ctypes.CDLL(None, use_errno=True)
        libc.setns.argtypes = (ctypes.c_int, ctypes.c_int)
        libc.setns.restype = ctypes.c_int
        for name, kind in (("mnt", 0x20000), ("pid", 0x20000000)):
            if libc.setns(handles[name], kind):
                raise OSError(ctypes.get_errno(), "setns " + name)
        os.fchdir(root)
        os.chroot(".")
        os.chdir("/")
        alive()
        child = os.fork()
        if child == 0:
            try:
                for name, fd in handles.items():
                    current = os.open("/proc/self/ns/" + name, os.O_RDONLY | os.O_CLOEXEC)
                    try:
                        if identity(current) != identity(fd):
                            refused("namespace/root selection is inconsistent: " + name)
                    finally:
                        os.close(current)
                for arguments in plan["mounts"]:
                    alive()
                    result = subprocess.run(
                        ["/usr/bin/mount", *arguments], capture_output=True, text=True,
                        env={"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"},
                    )
                    if result.returncode:
                        raise RuntimeError("cargo_overlay_mount_refused: " + result.stderr.strip())
                    alive()
                os._exit(0)
            except Exception as error:
                print(str(error), file=sys.stderr, flush=True)
                os._exit(1)
        _, status = os.waitpid(child, 0)
        alive()
        if not os.WIFEXITED(status) or os.WEXITSTATUS(status):
            raise RuntimeError("cargo_overlay_mount_refused: pinned helper failed")
    finally:
        for fd in reversed(descriptors):
            os.close(fd)


try:
    setup(json.loads(sys.argv[1]))
except Exception as error:
    print(str(error), file=sys.stderr)
    sys.exit(1)
