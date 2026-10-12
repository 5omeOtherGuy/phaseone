#!/usr/bin/env python3
"""scripts/fetch-connectors.py: the distribution composes only the connector release the lock
pins, asset by asset, and unpacks nothing from it but catalog data and package archives (#893)."""
import hashlib
import io
import json
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / 'fetch-connectors.py'
CATALOG = 'connectors-catalog.tar.gz'
PACKAGE = 'connectors-claude-code-oauth.p1pkg'
COMPONENT = b'\0asm component bytes'


def p1pkg(name: str = 'connectors/claude-code-oauth', component: bytes = COMPONENT,
          members: tuple[str, ...] = ('manifest.json', 'component.wasm')) -> bytes:
    """A package archive as pack-module.py writes one."""
    manifest = {'format': 'p1-module-package/1', 'name': name, 'version': '1.0.0',
                'kind': 'credential-method', 'world': 'p1:module/credential-method@1.2.0',
                'protocol': '1.0', 'capabilities': [], 'variant': 'default',
                'digest': digest(COMPONENT), 'size': len(COMPONENT),
                'token_origins': ['https://auth.example']}
    data = {'manifest.json': json.dumps(manifest).encode(), 'component.wasm': component}
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode='w', format=tarfile.USTAR_FORMAT) as archive:
        for member in members:
            info = tarfile.TarInfo(member)
            info.size = len(data[member])
            archive.addfile(info, io.BytesIO(data[member]))
    return buffer.getvalue()


def tar_gz(members: dict[str, bytes], symlink: str | None = None) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
        for name, content in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
        if symlink:
            info = tarfile.TarInfo(symlink)
            info.type = tarfile.SYMTYPE
            info.linkname = '/etc/passwd'
            archive.addfile(info)
    return buffer.getvalue()


def digest(data: bytes) -> str:
    return 'sha256:' + hashlib.sha256(data).hexdigest()


class Release:
    """A connector release on disk, served over file://, and a lock pinning it."""

    def __init__(self, root: pathlib.Path, catalog: bytes | None = None, package: bytes | None = None):
        self.root = root
        self.dir = root / 'release'
        self.dir.mkdir()
        self.assets = {
            CATALOG: catalog if catalog is not None else tar_gz({
                'routes/anthropic-subscription.toml': b'id = "anthropic-subscription"\n',
                'profiles/glm-5.3-flash-clinepass.toml': b'id = "glm-5.3-flash-clinepass"\n',
            }),
            PACKAGE: package if package is not None else p1pkg(),
        }
        for name, data in self.assets.items():
            (self.dir / name).write_bytes(data)
        (self.dir / 'SHA256SUMS').write_text(''.join(
            f'{digest(data)[7:]}  {name}\n' for name, data in self.assets.items()))
        self.lock = root / 'connectors.lock'
        self.lock.write_text('# The pinned connector release.\nrepository = "acme/connectors"\n'
                             'tag = "v1.0.0"\n\n[assets]\n' + ''.join(
                                 f'"{name}" = "{digest(data)}"\n' for name, data in self.assets.items()))

    def run(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(SCRIPT), '--lock', str(self.lock),
                               '--base-url', self.dir.as_uri(), *args],
                              capture_output=True, text=True)


class FetchConnectorsTest(unittest.TestCase):
    def test_the_pinned_release_unpacks_into_a_catalog_root_packages_and_build_outputs(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            out = pathlib.Path(temp) / 'out'
            result = release.run('--out', str(out))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('acme/connectors v1.0.0: 2 catalog files, 1 packages', result.stdout)
            self.assertEqual((out / 'catalog/routes/anthropic-subscription.toml').read_bytes(),
                             b'id = "anthropic-subscription"\n')
            self.assertTrue((out / 'catalog/profiles/glm-5.3-flash-clinepass.toml').is_file())
            self.assertEqual((out / 'packages' / PACKAGE).read_bytes(), release.assets[PACKAGE])
            # The build layout: the component and the build manifest, without the archive's own
            # format and version, under the package name with `/` as `-`.
            built = out / 'modules/connectors-claude-code-oauth'
            self.assertEqual(sorted(path.name for path in built.iterdir()),
                             ['connectors-claude-code-oauth.manifest.json',
                              'connectors-claude-code-oauth.sha256',
                              'connectors-claude-code-oauth.wasm'])
            self.assertEqual((built / 'connectors-claude-code-oauth.wasm').read_bytes(), COMPONENT)
            self.assertEqual((built / 'connectors-claude-code-oauth.sha256').read_text(),
                             f'{digest(COMPONENT)[7:]}  connectors-claude-code-oauth.wasm\n')
            manifest = json.loads((built / 'connectors-claude-code-oauth.manifest.json').read_text())
            self.assertEqual(manifest['name'], 'connectors/claude-code-oauth')
            self.assertEqual(manifest['token_origins'], ['https://auth.example'])
            self.assertNotIn('format', manifest)
            self.assertNotIn('version', manifest)
            self.assertEqual((out / 'connectors.lock').read_bytes(), release.lock.read_bytes())

    def test_a_fetch_by_the_same_lock_is_up_to_date_and_another_lock_replaces_it_whole(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            out = pathlib.Path(temp) / 'out'
            self.assertEqual(release.run('--out', str(out)).returncode, 0)
            (out / 'catalog/routes/stale.toml').write_text('id = "stale"\n')
            # Nothing is downloaded: the release is gone and the fetch still succeeds.
            for asset in release.dir.iterdir():
                asset.unlink()
            again = release.run('--out', str(out))
            self.assertEqual(again.returncode, 0, again.stderr)
            self.assertIn('is up to date', again.stdout)
            next_root = pathlib.Path(temp) / 'next'
            next_root.mkdir()
            release = Release(next_root, catalog=tar_gz({'routes/glm-subscription.toml': b''}))
            moved = release.run('--out', str(out))
            self.assertEqual(moved.returncode, 0, moved.stderr)
            self.assertFalse((out / 'catalog/routes/stale.toml').exists())
            self.assertEqual((out / 'connectors.lock').read_bytes(), release.lock.read_bytes())

    def test_a_directory_that_is_no_earlier_fetch_is_never_replaced(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            out = pathlib.Path(temp) / 'out'
            out.mkdir()
            (out / 'keep').write_text('mine')
            result = release.run('--out', str(out))
            self.assertEqual(result.returncode, 1)
            self.assertIn('not an earlier fetch', result.stderr)
            self.assertEqual((out / 'keep').read_text(), 'mine')

    def test_a_package_archive_that_is_not_the_frozen_format_is_refused(self):
        for package, message in (
                (p1pkg(component=b'other bytes'), 'differs from the digest and size'),
                (p1pkg(name='p1/read'), 'outside p1/'),
                (p1pkg(members=('manifest.json',)), 'holds exactly'),
                (b'not a tar archive at all', PACKAGE)):
            with self.subTest(message=message), tempfile.TemporaryDirectory() as temp:
                release = Release(pathlib.Path(temp), package=package)
                out = pathlib.Path(temp) / 'out'
                result = release.run('--out', str(out))
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(message, result.stderr)
                self.assertFalse(out.exists())

    def test_an_asset_that_differs_from_its_pin_is_refused_and_nothing_is_written(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            (release.dir / PACKAGE).write_bytes(b'ustar package bytes, changed')
            out = pathlib.Path(temp) / 'out'
            result = release.run('--out', str(out))
            self.assertEqual(result.returncode, 1)
            self.assertIn(f'{PACKAGE}: digest', result.stderr)
            self.assertIn('differs from the pinned', result.stderr)
            self.assertFalse(out.exists())

    def test_a_catalog_member_outside_routes_and_profiles_refuses_the_whole_archive(self):
        for members, symlink in (({'routes/a.toml': b'', 'accounts/claude.toml': b''}, None),
                                 ({'routes/../../escape.toml': b''}, None),
                                 ({'routes/a.toml': b''}, 'routes/passwd.toml')):
            with self.subTest(members=list(members), symlink=symlink), \
                    tempfile.TemporaryDirectory() as temp:
                release = Release(pathlib.Path(temp), catalog=tar_gz(members, symlink))
                out = pathlib.Path(temp) / 'out'
                result = release.run('--out', str(out))
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn('is not a routes/ or profiles/ file', result.stderr)
                self.assertFalse((out / 'catalog').exists())

    def test_a_lock_without_the_catalog_or_with_a_loose_digest_is_a_rejected_lock(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            for body, message in (
                    ('repository = "a/b"\ntag = "v1"\n[assets]\n"x.p1pkg" = "sha256:' + '0' * 64 + '"\n',
                     f'must pin {CATALOG}'),
                    (f'repository = "a/b"\ntag = "v1"\n[assets]\n"{CATALOG}" = "abc"\n',
                     'must pin `sha256:<64 hex>`')):
                release.lock.write_text(body)
                result = release.run('--out', str(pathlib.Path(temp) / 'out'))
                self.assertEqual(result.returncode, 1)
                self.assertIn(message, result.stderr)

    def test_pinning_a_tag_rewrites_the_lock_from_the_release_sums_and_keeps_the_comments(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            release.lock.write_text('# The pinned connector release.\nrepository = "acme/connectors"\n'
                                    'tag = "v0.9.0"\n\n[assets]\n"' + CATALOG + '" = "sha256:' + '0' * 64 + '"\n')
            result = release.run('--pin', 'v1.0.0')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('pinned acme/connectors v1.0.0: 2 assets', result.stdout)
            text = release.lock.read_text()
            self.assertTrue(text.startswith('# The pinned connector release.\n'))
            self.assertIn('tag = "v1.0.0"', text)
            self.assertIn(f'"{PACKAGE}" = "{digest(release.assets[PACKAGE])}"', text)
            fetched = release.run('--out', str(pathlib.Path(temp) / 'out'))
            self.assertEqual(fetched.returncode, 0, fetched.stderr)


if __name__ == '__main__':
    unittest.main()
