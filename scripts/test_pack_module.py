#!/usr/bin/env python3
"""scripts/pack-module.py: a package archive holds the build's component and manifest, and
nothing it does not verify (ADR-0209)."""
import hashlib
import io
import json
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / 'pack-module.py'
PACKAGE = 'acme-module-echo'
COMPONENT = b'\0asm\x0d\0\x01\0component bytes'


def build(root: pathlib.Path, component: bytes = COMPONENT, **fields) -> pathlib.Path:
    """A package build output directory as scripts/build-modules.sh publishes one."""
    directory = root / PACKAGE
    directory.mkdir()
    manifest = {
        'name': 'acme/echo',
        'kind': 'tool',
        'world': 'p1:module/tool@1.0.0',
        'protocol': '1.0',
        'capabilities': ['control'],
        'variant': 'default',
        'digest': 'sha256:' + hashlib.sha256(COMPONENT).hexdigest(),
        'size': len(COMPONENT),
    }
    manifest.update(fields)
    (directory / f'{PACKAGE}.manifest.json').write_text(json.dumps(manifest))
    (directory / f'{PACKAGE}.wasm').write_bytes(component)
    return directory


def pack(directory: pathlib.Path, output: pathlib.Path) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(SCRIPT), str(directory), '--version', '2.0.1',
                           '--output', str(output)], capture_output=True, text=True)


class PackModuleTest(unittest.TestCase):
    def test_the_archive_holds_the_manifest_and_the_component_reproducibly(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            directory = build(root)
            first, second = root / 'a.p1pkg', root / 'b.p1pkg'
            for output in (first, second):
                done = pack(directory, output)
                self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first, format=tarfile.USTAR_FORMAT) as tar:
                self.assertEqual(tar.getnames(), ['manifest.json', 'component.wasm'])
                self.assertTrue(all(member.isreg() and member.mtime == 0 for member in tar))
                manifest = json.load(tar.extractfile('manifest.json'))
                component = tar.extractfile('component.wasm').read()
            self.assertEqual(component, COMPONENT)
            self.assertEqual(manifest['format'], 'p1-module-package/1')
            self.assertEqual(manifest['version'], '2.0.1')
            self.assertEqual(manifest['name'], 'acme/echo')
            self.assertEqual(manifest['digest'], 'sha256:' + hashlib.sha256(COMPONENT).hexdigest())

    def test_bytes_the_manifest_does_not_pin_are_refused(self):
        for component, fields, why in (
            (COMPONENT + b'x', {}, 'pins'),
            (COMPONENT, {'size': 1}, 'bytes'),
            (COMPONENT, {'extra': 1}, 'fields'),
        ):
            with self.subTest(why=why), tempfile.TemporaryDirectory() as temp:
                root = pathlib.Path(temp)
                output = root / 'out.p1pkg'
                done = pack(build(root, component, **fields), output)
                self.assertEqual(done.returncode, 1)
                self.assertIn(why, done.stderr)
                self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
