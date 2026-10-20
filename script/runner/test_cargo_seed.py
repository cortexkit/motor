#!/usr/bin/env python3
"""Cargo seed integrity and real offline Cargo resolution, without public downloads."""

import hashlib
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('cargo_seed', Path(__file__).with_name('cargo-seed.py'))
if SPEC is None or SPEC.loader is None:
    raise ImportError('cannot load Cargo seed inventory')
seed = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(seed)
PROBE_SPEC = importlib.util.spec_from_file_location('cargo_offline_probe', Path(__file__).with_name('cargo-offline-probe.py'))
if PROBE_SPEC is None or PROBE_SPEC.loader is None:
    raise ImportError('cannot load offline Cargo acceptance probe')
offline_probe = importlib.util.module_from_spec(PROBE_SPEC)
PROBE_SPEC.loader.exec_module(offline_probe)
REGISTRY = 'index.crates.io-1949cf8c6b5b557f'


def cached_crate(home, name, version):
    filename = f'{name}-{version}'
    payload = io.BytesIO()
    with tarfile.open(fileobj=payload, mode='w:gz') as archive:
        for path, content in {'Cargo.toml': f'[package]\nname="{name}"\nversion="{version}"\nedition="2021"\n',
                              'src/lib.rs': 'pub fn value() -> u32 { 7 }\n'}.items():
            member = tarfile.TarInfo(filename + '/' + path)
            data = content.encode()
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    data = payload.getvalue()
    path = home / 'registry/cache' / REGISTRY / (filename + '.crate')
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    index = home / 'registry/index' / REGISTRY
    index.mkdir(parents=True, exist_ok=True)
    (index / 'config.json').write_text(json.dumps({'dl': 'https://static.crates.io/crates', 'api': 'https://crates.io'}))
    row = {'name': name, 'vers': version, 'deps': [], 'cksum': hashlib.sha256(data).hexdigest(),
           'features': {}, 'yanked': False}
    cache = index / '.cache' / seed.index_path(name)
    cache.parent.mkdir(parents=True, exist_ok=True)
    cache.write_bytes(b'\x03\x02\x00\x00\x00etag: "fixture"\x00' + version.encode() + b'\x00' + json.dumps(row).encode() + b'\x00')


class CargoSeedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / 'server'
        cached_crate(self.home, 'cortexkit-cow', '0.1.0')
        cached_crate(self.home, 'cortexkit-bus-testkit', '0.1.3')
        git = self.home / 'git/db/private-fleet/objects/pack'
        git.mkdir(parents=True)
        (git / 'pack-fixture.pack').write_bytes(b'private fleet object fixture')
        (git.parent.parent / 'config').write_text('credential.helper=SECRET\nurl=https://SECRET@example.invalid')
        (self.home / 'credentials.toml').write_text('SECRET')
        self.archive = self.root / 'seed.tar.gz'
        self.manifest = seed.export(self.home, self.archive)
        self.manifest_path = self.root / 'seed.json'
        self.save_manifest()

    def save_manifest(self):
        self.manifest_path.write_text(json.dumps(self.manifest))
        self.sha256 = seed.digest(self.manifest_path)

    def install(self):
        home = self.root / 'boat'
        receipt = seed.install(self.manifest_path, self.archive, self.sha256, home)
        self.assertEqual(receipt['crate_count'], 2)
        self.assertEqual(receipt['captured_at'], self.manifest['captured_at'])
        return home

    def test_capture_includes_crate_with_no_lockfile_entry(self):
        self.assertEqual(self.manifest['crate_count'], 2)
        self.assertEqual({c['name'] for c in self.manifest['crates']}, {'cortexkit-cow', 'cortexkit-bus-testkit'})
        self.assertEqual(list(self.home.rglob('Cargo.lock')), [])
        self.assertTrue(self.manifest['captured_at'].endswith('+00:00'))

    def test_seed_excludes_credentials_config_sources_and_git_checkouts(self):
        with tarfile.open(self.archive) as archive:
            names = archive.getnames()
        self.assertIn('git/db/private-fleet/objects/pack/pack-fixture.pack', names)
        self.assertFalse(any('/config' in name and not name.endswith('/config.json') for name in names))
        self.assertFalse(any('credentials' in name or '/src/' in name or 'checkouts' in name for name in names))

    def test_seed_supports_nested_helper_and_no_lockfile_offline_on_readonly_registry(self):
        home = self.install()
        registry = home / 'registry'
        paths = [registry, *registry.rglob('*')]
        for path in paths:
            path.chmod(0o555 if path.is_dir() else 0o444)
        try:
            output = io.StringIO()
            with patch.dict(os.environ, CARGO_HOME=str(home)), contextlib.redirect_stdout(output):
                offline_probe.run_probe()
            report = json.loads(output.getvalue())
            self.assertTrue(report['passed'])
            self.assertEqual([(row['case'], row['crate'], row['version']) for row in report['checks']],
                             [('nested-helper-lockfile', 'cortexkit-cow', '0.1.0'),
                              ('no-lockfile-entry', 'cortexkit-bus-testkit', '0.1.3')])
        finally:
            for path in paths:
                path.chmod(0o755 if path.is_dir() else 0o644)

    def test_missing_seed_crate_fails_parity_with_count_and_example(self):
        home = self.install()
        (home / 'registry/cache' / REGISTRY / 'cortexkit-bus-testkit-0.1.3.crate').unlink()
        result = seed.differences(self.manifest, seed.capture(home))
        self.assertFalse(result['passed'])
        self.assertEqual(result['missing_crates'], 1)
        self.assertEqual(result['missing_examples'], [REGISTRY + '/cortexkit-bus-testkit 0.1.3'])

    def test_git_database_seed_supports_trusted_offline_fetch_without_source_config(self):
        repo = self.root / 'git-source'
        (repo / 'src').mkdir(parents=True)
        (repo / 'src/lib.rs').write_text('pub fn git_value() {}')
        (repo / 'Cargo.toml').write_text('[package]\nname="git-seed-probe"\nversion="0.1.0"\nedition="2021"\n')
        env = dict(os.environ, RUSTC_WRAPPER='', CARGO_BUILD_RUSTC_WRAPPER='',
                   CARGO_HOME=str(self.home), CARGO_NET_OFFLINE='false')
        for key in ('GIT_CONFIG_COUNT', 'GIT_CONFIG_KEY_0', 'GIT_CONFIG_VALUE_0'):
            env.pop(key, None)
        for args in (['init', '-q'], ['add', '.'], ['-c', 'user.name=Seed fixture', '-c',
                     'user.email=seed@invalid', 'commit', '-qm', 'fixture']):
            subprocess.run(['git', *args], cwd=repo, env=env, check=True, capture_output=True)
        consumer = self.root / 'consumer'
        (consumer / 'src').mkdir(parents=True)
        (consumer / 'src/lib.rs').write_text('pub fn consumer() {}')
        (consumer / 'Cargo.toml').write_text('[package]\nname="consumer"\nversion="0.1.0"\nedition="2021"\n'
                                             f'[dependencies]\ngit-seed-probe={{git="{repo.as_uri()}"}}\n')
        subprocess.run(['cargo', 'fetch'], cwd=consumer, env=env, check=True, capture_output=True)
        self.archive.unlink()
        self.manifest = seed.export(self.home, self.archive)
        self.save_manifest()
        home = self.install()
        self.assertFalse((home / 'git/checkouts').exists())
        env.update(CARGO_HOME=str(home), CARGO_NET_OFFLINE='true')
        result = subprocess.run(['cargo', 'fetch', '--locked', '--offline'], cwd=consumer,
                                env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((home / 'git/checkouts').is_dir())

    def test_missing_index_metadata_fails_parity(self):
        home = self.install()
        (home / 'registry/index' / REGISTRY / '.cache' / seed.index_path('cortexkit-cow')).unlink()
        result = seed.differences(self.manifest, seed.capture(home, strict=False))
        self.assertFalse(result['passed'])
        self.assertEqual(result['missing_crates'], 1)
        self.assertEqual(result['integrity_errors'], 1)

    def test_missing_git_object_fails_parity(self):
        home = self.install()
        (home / 'git/db/private-fleet/objects/pack/pack-fixture.pack').unlink()
        self.assertFalse(seed.differences(self.manifest, seed.capture(home))['passed'])

    def test_missing_index_config_fails_parity(self):
        home = self.install()
        (home / 'registry/index' / REGISTRY / 'config.json').unlink()
        result = seed.differences(self.manifest, seed.capture(home))
        self.assertFalse(result['passed'])
        self.assertEqual(result['missing_index_configs'], 1)

    def test_corrupt_crate_fails_index_checksum(self):
        (self.home / 'registry/cache' / REGISTRY / 'cortexkit-cow-0.1.0.crate').write_bytes(b'bad')
        with self.assertRaisesRegex(ValueError, 'index checksum mismatch: cortexkit-cow'):
            seed.capture(self.home)

    def test_legacy_git_registry_index_is_refused(self):
        (self.home / 'registry/index' / REGISTRY / '.git').mkdir()
        with self.assertRaisesRegex(ValueError, 'legacy Git registry index'):
            seed.capture(self.home)

    def test_manifest_digest_is_independently_verified(self):
        with self.assertRaisesRegex(ValueError, 'manifest SHA-256 mismatch'):
            seed.install(self.manifest_path, self.archive, '0' * 64, self.root / 'boat')

    def test_archive_digest_is_verified(self):
        with self.archive.open('ab') as output:
            output.write(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'archive SHA-256/size mismatch'):
            self.install()

    def test_member_hashes_are_reverified_inside_sandbox(self):
        name = next(iter(self.manifest['files']))
        with tarfile.open(self.archive, 'w:gz') as archive:
            for path, row in self.manifest['files'].items():
                data = (self.home / path).read_bytes()
                if path == name:
                    data = b'x' * len(data)
                member = tarfile.TarInfo(path)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
        self.manifest['archive'] = {'sha256': seed.digest(self.archive), 'bytes': self.archive.stat().st_size}
        self.save_manifest()
        with self.assertRaisesRegex(ValueError, 'member SHA-256 mismatch'):
            self.install()

    def test_unsafe_and_unexpected_paths_are_refused(self):
        for name in ('../outside', '/outside', 'git/db/private-fleet/config', 'credentials.toml',
                     'registry/index/id/.cache/../../../outside', 'git/db/id/objects/info/alternates'):
            self.assertFalse(seed.allowed(name), name)
        with tarfile.open(self.archive, 'w:gz') as archive:
            member = tarfile.TarInfo('../outside')
            member.size = 1
            archive.addfile(member, io.BytesIO(b'x'))
        self.manifest['archive'] = {'sha256': seed.digest(self.archive), 'bytes': self.archive.stat().st_size}
        self.save_manifest()
        with self.assertRaisesRegex(ValueError, 'unexpected seed archive member'):
            self.install()
        self.assertFalse((self.root / 'outside').exists())

    def test_symlink_and_external_git_alternates_sources_are_refused(self):
        (self.home / 'registry/cache/link').symlink_to('/tmp')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            seed.capture(self.home)
        (self.home / 'registry/cache/link').unlink()
        info = self.home / 'git/db/private-fleet/objects/info'
        info.mkdir()
        (info / 'alternates').write_text('/external/private/objects\n')
        with self.assertRaisesRegex(ValueError, 'external alternates'):
            seed.capture(self.home)


if __name__ == '__main__':
    unittest.main()
