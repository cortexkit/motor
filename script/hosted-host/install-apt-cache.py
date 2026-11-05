#!/usr/bin/env python3
"""Install a localhost-only Debian snapshot cache on an Ubuntu image-build host.

Run as root after /srv/build is mounted. For image builds, set
MOTOR_DEBIAN_MIRROR=http://127.0.0.1:3142/debian-snapshot/<snapshot>/
where <snapshot> is debian_snapshot from script/tenant-image/versions.txt.
"""

import argparse
import os
from pathlib import Path
import subprocess


BUILD = Path('/srv/build')
CONFIG = Path('/etc/apt-cacher-ng/zz-motor-snapshot.conf')
CACHE = BUILD / 'apt-cacher-ng'
UNIT = Path('/etc/systemd/system/apt-cacher-ng.service.d/motor-cache.conf')
PACKAGES = ('apt-cacher-ng', 'ca-certificates')


def configuration():
    # HTTP on loopback lets the proxy cache the response body. CONNECT would
    # tunnel HTTPS without caching it; the remap instead uses HTTPS upstream.
    # debootstrap and apt still require Debian's signed Release metadata and
    # authenticated package hashes, independently of the proxy's TLS checks.
    # Dated dists metadata is immutable too: mark it solid to avoid upstream
    # revalidation on warm builds. Whitelist the snapshot's entire cache tree
    # against expiration, including packages no longer referenced by an index.
    # Operators reclaim old snapshots explicitly; this cache has no size cap.
    return f'''# Managed by script/hosted-host/install-apt-cache.py
BindAddress: 127.0.0.1
Port: 3142
CacheDir: {CACHE}
Remap-debian-snapshot: /debian-snapshot ; https://snapshot.debian.org/archive/debian
SPfilePatternEx: ^/?(debian-snapshot/|archive/debian/)?[0-9]{{8}}T[0-9]{{6}}Z/dists/
WfilePatternEx: ^debian-snapshot/[0-9]{{8}}T[0-9]{{6}}Z/
NoSSLChecks: 0
'''


def service_configuration():
    return f'''[Unit]
RequiresMountsFor={CACHE}
ConditionPathIsMountPoint={BUILD}

[Service]
ReadWritePaths={CACHE}
'''


def install(run=subprocess.run):
    # Fail before installing anything if the build volume is absent: the cache
    # must not silently consume the host's root filesystem.
    run(['mountpoint', '-q', str(BUILD)], check=True)
    missing = []
    for package in PACKAGES:
        status = run(['dpkg-query', '-W', '-f=${db:Status-Status}', package],
                     capture_output=True, text=True)
        if status.returncode != 0 or status.stdout != 'installed':
            missing.append(package)
    desired = {CONFIG: configuration(), UNIT: service_configuration()}
    changed = {path: text for path, text in desired.items() if not path.exists() or path.read_text() != text}
    if missing or changed:
        # A package's post-install start must never expose its default listener
        # on all interfaces before the loopback configuration is in place.
        run(['systemctl', 'mask', '--now', 'apt-cacher-ng.service'], check=True)
    if missing:
        env = dict(os.environ, DEBIAN_FRONTEND='noninteractive', NEEDRESTART_MODE='l')
        run(['apt-get', '-o', 'Acquire::Retries=3', 'update'], check=True, env=env)
        run(['apt-get', '-o', 'Acquire::Retries=3', 'install', '-y', '--no-install-recommends', *missing],
            check=True, env=env)
    run(['install', '-d', '-o', 'apt-cacher-ng', '-g', 'apt-cacher-ng', '-m', '0750', str(CACHE)], check=True)
    for path, text in changed.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix('.pending')
        temporary.write_text(text)
        temporary.chmod(0o644)
        temporary.replace(path)
    if changed:
        run(['systemctl', 'daemon-reload'], check=True)
    run(['systemctl', 'unmask', 'apt-cacher-ng.service'], check=True)
    run(['systemctl', 'enable', '--now', 'apt-cacher-ng.service'], check=True)
    print(f'apt snapshot cache ready: 127.0.0.1:3142; cache at {CACHE}')


if __name__ == '__main__':
    argparse.ArgumentParser(description=__doc__).parse_args()
    if os.geteuid() != 0 or os.uname().sysname != 'Linux':
        raise SystemExit('run as root on the Ubuntu image-build host')
    install()
