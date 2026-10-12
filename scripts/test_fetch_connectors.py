#!/usr/bin/env python3
"""scripts/fetch-connectors.py: the distribution composes only the connector release the lock
pins, asset by asset, and unpacks nothing but catalog data from it (#893)."""
import hashlib
import io
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / 'fetch-connectors.py'
CATALOG = 'connectors-catalog.tar.gz'
PACKAGE = 'subscriptions-claude-code-oauth.p1pkg'


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

    def __init__(self, root: pathlib.Path, catalog: bytes | None = None):
        self.root = root
        self.dir = root / 'release'
        self.dir.mkdir()
        self.assets = {
            CATALOG: catalog if catalog is not None else tar_gz({
                'routes/anthropic-subscription.toml': b'id = "anthropic-subscription"\n',
                'profiles/glm-5.3-flash-clinepass.toml': b'id = "glm-5.3-flash-clinepass"\n',
            }),
            PACKAGE: b'ustar package bytes',
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
    def test_the_pinned_release_unpacks_into_a_catalog_root_and_packages(self):
        with tempfile.TemporaryDirectory() as temp:
            release = Release(pathlib.Path(temp))
            out = pathlib.Path(temp) / 'out'
            result = release.run('--out', str(out))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('acme/connectors v1.0.0: 2 catalog files, 1 packages', result.stdout)
            self.assertEqual((out / 'catalog/routes/anthropic-subscription.toml').read_bytes(),
                             b'id = "anthropic-subscription"\n')
            self.assertTrue((out / 'catalog/profiles/glm-5.3-flash-clinepass.toml').is_file())
            self.assertEqual((out / 'packages' / PACKAGE).read_bytes(), b'ustar package bytes')

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
