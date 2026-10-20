#!/usr/bin/env python3
"""Operator-supplied, tenant-1-only offline Cargo seed; never distribute to customers."""

import argparse
from datetime import datetime, timezone
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import tarfile
import tempfile
import time


ROOTS = ('registry/index', 'registry/cache', 'git/db')


def private_create(path):
    return os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb')


def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def allowed(name):
    path = PurePosixPath(name)
    if path.is_absolute() or '..' in path.parts or str(path) != name:
        return False
    parts = path.parts
    if len(parts) == 4 and parts[:2] == ('registry', 'cache'):
        return bool(re.fullmatch(r'[A-Za-z0-9_.+-]+\.crate', parts[3]))
    if len(parts) >= 4 and parts[:2] == ('registry', 'index'):
        return parts[3] == '.cache' or (len(parts) == 4 and parts[3] == 'config.json')
    if len(parts) >= 4 and parts[:2] == ('git', 'db'):
        return (len(parts) == 4 and parts[3] in ('HEAD', 'packed-refs', 'shallow')
                or len(parts) > 4 and parts[3] in ('objects', 'refs')
                and parts[4:] != ('info', 'alternates'))
    return False


def index_path(name):
    name = name.lower()
    if len(name) <= 2:
        return f'{len(name)}/{name}'
    if len(name) == 3:
        return f'3/{name[0]}/{name}'
    return f'{name[:2]}/{name[2:4]}/{name}'


def index_checksum(home, registry, name, version):
    index = home / 'registry/index' / registry
    if (index / '.git').exists():
        raise ValueError('legacy Git registry index cannot be seeded as sparse metadata: ' + registry)
    path = index / '.cache' / index_path(name)
    # Cargo's sparse cache has a binary header and NUL-delimited JSON records.
    for record in path.read_bytes().split(b'\0'):
        if record.startswith(b'{'):
            row = json.loads(record)
            if row.get('name') == name and row.get('vers') == version:
                return row['cksum']
    raise ValueError(f'missing sparse index entry: {registry}/{name} {version}')


def capture(home, strict=True):
    home = Path(home)
    files, crates, errors = {}, [], []
    for root in ROOTS:
        for path in sorted((home / root).rglob('*')):
            name = path.relative_to(home).as_posix()
            if path.is_symlink():
                raise ValueError('seed source contains symlink: ' + name)
            if path.is_file() and allowed(name):
                files[name] = {'sha256': digest(path), 'bytes': path.stat().st_size}
            if name.endswith('/objects/info/alternates') and path.read_text().strip():
                raise ValueError('Git seed depends on external alternates: ' + name)
    for name, spec in files.items():
        if name.startswith('registry/cache/'):
            registry, filename = PurePosixPath(name).parts[2:]
            match = re.fullmatch(r'(.+)-(\d+\.\d+\.\d+[^/]*)\.crate', filename)
            if not match:
                raise ValueError('invalid crate filename: ' + name)
            crate, version = match.groups()
            try:
                checksum = index_checksum(home, registry, crate, version)
                if checksum != spec['sha256']:
                    raise ValueError(f'index checksum mismatch: {crate} {version}')
            except (OSError, ValueError) as error:
                if strict:
                    raise
                errors.append(str(error))
                continue
            crates.append({'registry': registry, 'name': crate, 'version': version,
                           'checksum': checksum, 'bytes': spec['bytes']})
    return {'schema': 1, 'scope': 'tenant-1-private-fleet',
            'captured_at': datetime.now(timezone.utc).isoformat(),
            'crate_count': len(crates), 'crates': crates, 'files': files,
            'integrity_errors': errors,
            'payload_bytes': sum(row['bytes'] for row in files.values())}


def validate(manifest):
    if manifest['schema'] != 1 or manifest['scope'] != 'tenant-1-private-fleet':
        raise ValueError('seed is not a tenant-1 fleet inventory')
    if manifest['crate_count'] != len(manifest['crates']) or not manifest['crates']:
        raise ValueError('empty or inconsistent crate inventory')
    for name, row in manifest['files'].items():
        if not allowed(name) or not re.fullmatch(r'[a-f0-9]{64}', row['sha256']) or row['bytes'] < 0:
            raise ValueError('invalid seed member: ' + name)
    for crate in manifest['crates']:
        path = f"registry/cache/{crate['registry']}/{crate['name']}-{crate['version']}.crate"
        if manifest['files'].get(path) != {'sha256': crate['checksum'], 'bytes': crate['bytes']}:
            raise ValueError('crate inventory disagrees with files: ' + path)


def differences(expected, actual):
    wanted = {(c['registry'], c['name'], c['version']): c for c in expected['crates']}
    have = {(c['registry'], c['name'], c['version']): c for c in actual['crates']}
    missing = sorted(set(wanted) - set(have))
    changed = sorted(key for key in set(wanted) & set(have)
                     if wanted[key]['checksum'] != have[key]['checksum'])
    git_missing = [name for name, row in expected['files'].items() if name.startswith('git/db/')
                   and actual['files'].get(name) != row]
    config_missing = [name for name, row in expected['files'].items() if name.endswith('/config.json')
                      and actual['files'].get(name) != row]
    integrity = actual.get('integrity_errors', [])
    return {'passed': not (missing or changed or git_missing or config_missing or integrity), 'required_crates': len(wanted),
            'observed_crates': len(have), 'missing_crates': len(missing),
            'missing_examples': ['%s/%s %s' % key for key in missing[:10]],
            'changed_crates': len(changed), 'changed_examples': ['%s/%s %s' % key for key in changed[:10]],
            'missing_git_files': len(git_missing), 'git_examples': git_missing[:10],
            'missing_index_configs': len(config_missing), 'config_examples': config_missing[:10],
            'integrity_errors': len(integrity), 'integrity_examples': integrity[:10]}


def export(home, archive):
    start = time.monotonic()
    manifest = capture(home)
    validate(manifest)
    # Hash the exported bytes, not just the source before a concurrent prefetch.
    with private_create(archive) as destination, tarfile.open(fileobj=destination, mode='w:gz', compresslevel=1) as output:
        for name, row in manifest['files'].items():
            path = Path(home) / name
            data = path.read_bytes()
            if len(data) != row['bytes'] or hashlib.sha256(data).hexdigest() != row['sha256']:
                raise ValueError('seed source changed during capture: ' + name)
            member = tarfile.TarInfo(name)
            member.size, member.mode = len(data), 0o644
            output.addfile(member, io.BytesIO(data))
    manifest['archive'] = {'sha256': digest(archive), 'bytes': Path(archive).stat().st_size}
    manifest['export_seconds'] = round(time.monotonic() - start, 3)
    return manifest


def verified_manifest(path, sha256):
    if digest(path) != sha256:
        raise ValueError('seed manifest SHA-256 mismatch')
    manifest = json.loads(Path(path).read_text())
    validate(manifest)
    return manifest


def extract_files(archive, destination, members):
    seen = set()
    with tarfile.open(archive, 'r:gz') as source:
        for member in source:
            row = members.get(member.name)
            if not member.isfile() or row is None or member.name in seen or member.size != row['bytes']:
                raise ValueError('unexpected seed archive member: ' + member.name)
            seen.add(member.name)
            path = destination / member.name
            path.parent.mkdir(parents=True, exist_ok=True)
            reader = source.extractfile(member)
            if reader is None:
                raise ValueError('seed member has no payload: ' + member.name)
            with reader, path.open('xb') as writer:
                shutil.copyfileobj(reader, writer)
            if digest(path) != row['sha256']:
                raise ValueError('seed member SHA-256 mismatch: ' + member.name)
    if seen != set(members):
        raise ValueError('seed archive is missing members')


def materialize_sources(home, manifest):
    # Job mounts are read-only: unpack the regenerable sources before snapshotting.
    for crate in manifest['crates']:
        name = f"{crate['name']}-{crate['version']}"
        source = home / 'registry/cache' / crate['registry'] / (name + '.crate')
        target = home / 'registry/src' / crate['registry']
        with tarfile.open(source, 'r:gz') as archive:
            for member in archive:
                path = PurePosixPath(member.name)
                if (path.is_absolute() or '..' in path.parts or not path.parts or path.parts[0] != name
                        or not (member.isfile() or member.isdir())):
                    raise ValueError('unsafe crate member: ' + member.name)
                output = target / member.name
                if member.isdir():
                    output.mkdir(parents=True, exist_ok=True)
                else:
                    output.parent.mkdir(parents=True, exist_ok=True)
                    reader = archive.extractfile(member)
                    if reader is None:
                        raise ValueError('crate member has no payload: ' + member.name)
                    with reader, output.open('wb') as writer:
                        shutil.copyfileobj(reader, writer)
                    output.chmod(0o755 if member.mode & 0o111 else 0o644)
        (target / name / '.cargo-ok').write_text('{"v":1}')


def install(manifest_path, archive, sha256, home):
    start = time.monotonic()
    manifest = verified_manifest(manifest_path, sha256)
    if digest(archive) != manifest['archive']['sha256'] or Path(archive).stat().st_size != manifest['archive']['bytes']:
        raise ValueError('seed archive SHA-256/size mismatch')
    home = Path(home)
    home.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='cargo-seed-', dir=home.parent) as scratch:
        stage = Path(scratch)
        extract_files(archive, stage, manifest['files'])
        measured = capture(stage)
        result = differences(manifest, measured)
        if not result['passed']:
            raise ValueError('seed verification failed: ' + json.dumps(result))
        materialize_sources(stage, manifest)
        source_bytes = sum(path.stat().st_size for path in (stage / 'registry/src').rglob('*') if path.is_file())
        for root in (*ROOTS, 'registry/src'):
            destination = home / root
            if destination.is_symlink():
                raise ValueError('seed destination is a symlink: ' + str(destination))
            if destination.exists():
                shutil.rmtree(destination)
            destination.parent.mkdir(parents=True, exist_ok=True)
            if (stage / root).exists():
                shutil.move(str(stage / root), destination)
    return {'manifest_sha256': sha256, 'captured_at': manifest['captured_at'],
            'crate_count': manifest['crate_count'], 'payload_bytes': manifest['payload_bytes'],
            'source_bytes': source_bytes, 'installed_bytes': source_bytes + manifest['payload_bytes'],
            'archive_bytes': manifest['archive']['bytes'], 'import_seconds': round(time.monotonic() - start, 3)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cargo-home', type=Path, default=Path('/srv/build/cargo-home'))
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--manifest-sha256')
    parser.add_argument('--acknowledge-tenant-one-private-cache', action='store_true')
    parser.add_argument('mode', choices=('export', 'install', 'check'))
    args = parser.parse_args()
    if args.mode in ('export', 'install') and (not args.archive or not args.acknowledge_tenant_one_private_cache):
        parser.error('export/install require --archive and --acknowledge-tenant-one-private-cache')
    if args.mode == 'export':
        result = export(args.cargo_home, args.archive)
        with private_create(args.manifest) as destination:
            destination.write((json.dumps(result, indent=2, sort_keys=True) + '\n').encode())
        result = {key: value for key, value in result.items() if key not in ('crates', 'files')}
        result['manifest_sha256'] = digest(args.manifest)
    else:
        if not args.manifest_sha256:
            parser.error('install/check require --manifest-sha256')
        manifest = verified_manifest(args.manifest, args.manifest_sha256)
        result = install(args.manifest, args.archive, args.manifest_sha256, args.cargo_home) if args.mode == 'install' else differences(manifest, capture(args.cargo_home, strict=False))
    print(json.dumps(result, indent=2))
    return 1 if result.get('passed') is False else 0


if __name__ == '__main__':
    raise SystemExit(main())
