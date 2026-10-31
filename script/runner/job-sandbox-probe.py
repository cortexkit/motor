#!/usr/bin/env python3
"""Observe a real Linux job and enforce docs/job-isolation.md section 4.4.2."""
import errno
import json
import os
from pathlib import Path
import pwd
import re
import stat
import sys
from typing import Any


CGROUP = re.compile(
    r"0::(/motor\.slice/motor-jobs\.slice/motor-job-"
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.service)\n?"
)
SUCCESS = "PASS job sandbox: cgroup, root ownership, memory.pressure ownership, read-only cgroup, PSI, bash PID 1 and motor-job identity"


def attempt(action):
    try:
        return action()
    except (OSError, KeyError) as error:
        return dict(error=str(error), errno=getattr(error, "errno", None))


def ownership(path):
    info = path.stat()
    return dict(uid=info.st_uid, gid=info.st_gid, mode=stat.S_IMODE(info.st_mode))


def pid1():
    status = dict(line.split(":", 1) for line in Path("/proc/1/status").read_text().splitlines())
    return dict(comm=Path("/proc/1/comm").read_text().strip(),
                uids=[int(value) for value in status["Uid"].split()],
                gids=[int(value) for value in status["Gid"].split()])


def identity():
    account = pwd.getpwnam("motor-job")
    return dict(name=pwd.getpwuid(os.geteuid()).pw_name,
                uid=os.getuid(), euid=os.geteuid(), gid=os.getgid(), egid=os.getegid(),
                account_uid=account.pw_uid, account_gid=account.pw_gid)


def pressure_write(path):
    # Opening for write must itself fail with EROFS. An invalid PSI trigger can
    # fail even on a writable mount, so a failed trigger write alone proves nothing.
    try:
        with path.open("w") as output:
            output.write("some 150000 1000000\n")
        return dict(wrote=True)
    except OSError as error:
        return dict(error=str(error), errno=error.errno)


def observe():
    result: dict[str, Any] = dict(
        identity=attempt(identity),
        cgroup=attempt(lambda: Path("/proc/self/cgroup").read_text()),
        pid1=attempt(pid1),
        system_pressure=attempt(lambda: Path("/proc/pressure/memory").read_text()),
    )
    match = CGROUP.fullmatch(result["cgroup"]) if isinstance(result["cgroup"], str) else None
    if match:
        group = Path("/sys/fs/cgroup" + match[1])
        result["directories"] = {
            str(path): attempt(lambda path=path: ownership(path))
            for path in (group, group.parent, group.parent.parent)
        }
        result["files"] = attempt(lambda: {
            path.name: ownership(path) for path in sorted(group.iterdir()) if path.is_file()
        })
        result["job_pressure"] = attempt(lambda: (group / "memory.pressure").read_text())
        result["pressure_write"] = pressure_write(group / "memory.pressure")
    return result


def require(condition, property_name, seen):
    if not condition:
        raise RuntimeError(f"job sandbox {property_name}: saw {seen!r}")


def judge(output):
    """Refuse incomplete observations as well as a measured contract mismatch."""
    try:
        seen = json.loads(output)
    except (ValueError, UnicodeError) as error:
        raise RuntimeError(f"job sandbox output: invalid JSON: {output!r}") from error
    require(isinstance(seen, dict), "output", seen)
    cgroup = seen.get("cgroup")
    match = CGROUP.fullmatch(cgroup) if isinstance(cgroup, str) else None
    require(match is not None, "cgroup path", cgroup)
    assert match is not None
    group = Path("/sys/fs/cgroup" + match[1])
    directories = seen.get("directories")
    require(isinstance(directories, dict), "root-owned cgroup directories", directories)
    for path in (group, group.parent, group.parent.parent):
        owner = directories.get(str(path))
        require(isinstance(owner, dict) and owner.get("uid") == 0 and owner.get("gid") == 0,
                "root-owned cgroup directory " + str(path), owner)

    for key, path in (("system_pressure", "/proc/pressure/memory"),
                      ("job_pressure", str(group / "memory.pressure"))):
        value = seen.get(key)
        require(isinstance(value, str) and bool(value.strip()), "PSI readable " + path, value)

    identity = seen.get("identity")
    require(isinstance(identity, dict), "motor-job identity", identity)
    uid, gid = identity.get("account_uid"), identity.get("account_gid")
    require(isinstance(uid, int) and uid > 0 and isinstance(gid, int) and gid > 0
            and identity.get("name") == "motor-job"
            and identity.get("uid") == identity.get("euid") == uid
            and identity.get("gid") == identity.get("egid") == gid,
            "motor-job identity", identity)

    # Check the exception explicitly, not a guessed uid or a file-count threshold:
    # systemd assigns only memory.pressure to the account, with mode 0644. Some
    # hosts' systemd leaves it root-owned (seen on Boat), which the contract also
    # accepts: the read-only mount checked below keeps it unwritable either way.
    files = seen.get("files")
    require(isinstance(files, dict) and bool(files) and "error" not in files,
            "cgroup control-file ownership", files)
    pressure = files.get("memory.pressure")
    require(isinstance(pressure, dict) and pressure.get("mode") == 0o644
            and (pressure.get("uid"), pressure.get("gid")) in ((uid, gid), (0, 0)),
            "memory.pressure owned by the job account or root, mode 0644", pressure)
    for name, owner in files.items():
        if name != "memory.pressure":
            require(isinstance(owner, dict) and owner.get("uid") == 0 and owner.get("gid") == 0,
                    "root-owned cgroup control file " + name, owner)

    write = seen.get("pressure_write")
    require(isinstance(write, dict) and write.get("errno") == errno.EROFS,
            "read-only cgroup (memory.pressure write must fail with EROFS)", write)
    init = seen.get("pid1")
    require(isinstance(init, dict) and init.get("comm") == "bash"
            and init.get("uids") == [uid] * 4 and init.get("gids") == [gid] * 4,
            "PID 1 bash wrapper as motor-job", init)
    return seen


def judge_job(parsed):
    seen = judge(parsed.get("stdout", ""))
    require(parsed.get("code") == 0, "probe job completion", parsed)
    return seen


def main():
    output = json.dumps(observe(), sort_keys=True)
    print(output, flush=True)
    try:
        judge(output)
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        return 1
    print(SUCCESS, file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
