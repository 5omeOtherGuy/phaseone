#!/usr/bin/env python3
"""An unreadable WIT digest must not yield a toolchain OK record."""
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / 'module-toolchain.sh'


class ToolchainDigestTest(unittest.TestCase):
    def test_sha256sum_failure_is_counted(self):
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / 'scripts').mkdir()
            shutil.copy2(SCRIPT, root / 'scripts' / 'module-toolchain.sh')
            (root / 'modules' / 'wit').mkdir(parents=True)
            (root / 'modules' / 'toolchain.pins').write_text('RUST_MIN=1.0.0\nWASM_TARGET=wasm32-unknown-unknown\n')
            (root / 'modules' / 'wit' / 'example.wit').write_text('world example {}\n')
            (root / 'sysroot' / 'lib' / 'rustlib' / 'wasm32-unknown-unknown').mkdir(parents=True)
            (root / 'bin').mkdir()
            for name, body in (
                ('rustc', f'''case "$*" in
  -vV) printf 'release: 1.90.0\\ncommit-hash: 1234\\n' ;;
  *) printf '%s\\n' '{root / 'sysroot'}' ;;
esac'''),
                ('cargo', 'echo cargo 1.90.0'),
                ('sha256sum', 'exit 9'),
            ):
                path = root / 'bin' / name
                path.write_text('#!/bin/sh\n' + body + '\n')
                path.chmod(0o755)
            env = dict(os.environ, PATH=str(root / 'bin') + ':' + os.environ['PATH'])
            done = subprocess.run(['bash', str(root / 'scripts' / 'module-toolchain.sh'), '--check'],
                                  env=env, capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0)
            self.assertNotIn('module-toolchain: OK', done.stdout)


if __name__ == '__main__':
    unittest.main()
