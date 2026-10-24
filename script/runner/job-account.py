#!/usr/bin/env python3
"""Create a role's job account without changing existing numeric identities."""

import grp
import pwd


def ensure(identity, run):
    if identity == 'system-assigned':
        ensure_system(run)
        return
    uid, gid = identity['uid'], identity['gid']
    for lookup, key, wanted in ((pwd.getpwuid, uid, 'motor-job'), (grp.getgrgid, gid, 'motor-job')):
        try:
            owner = lookup(key)[0]
        except KeyError:
            continue
        if owner != wanted:
            raise ValueError(f'motor-job numeric identity {key} already owned by {owner}')
    user, group = None, None
    try:
        user = pwd.getpwnam('motor-job')
    except KeyError:
        pass
    try:
        group = grp.getgrnam('motor-job')
    except KeyError:
        pass
    if user is not None and (user.pw_uid != uid or user.pw_gid != gid):
        raise ValueError(f'motor-job already has uid/gid {user.pw_uid}/{user.pw_gid}, expected {uid}/{gid}')
    if group is not None and group.gr_gid != gid:
        raise ValueError(f'motor-job group already has gid {group.gr_gid}, expected {gid}')
    if group is None:
        run('groupadd', '--system', '--gid', str(gid), 'motor-job')
    if user is None:
        run('useradd', '--system', '--uid', str(uid), '--gid', str(gid),
            '--home-dir', '/nonexistent', '--shell', '/usr/sbin/nologin', 'motor-job')


def ensure_system(run):
    try:
        group = grp.getgrnam('motor-job')
    except KeyError:
        group = None
    try:
        user = pwd.getpwnam('motor-job')
    except KeyError:
        user = None
    if user is not None and (group is None or user.pw_gid != group.gr_gid):
        raise ValueError('motor-job must have its own primary group')
    if group is None:
        run('groupadd', '--system', 'motor-job')
    if user is None:
        # Boat base images own their system IDs; let shadow choose unused ones.
        run('useradd', '--system', '--gid', 'motor-job', '--home-dir', '/nonexistent',
            '--shell', '/usr/sbin/nologin', 'motor-job')
