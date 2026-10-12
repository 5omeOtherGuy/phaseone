#!/usr/bin/env python3
"""The main helper cannot substitute an unrelated successful workflow for gate."""
import os
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / 'push-main.sh'


class PushMainTest(unittest.TestCase):
    def test_non_gate_success_does_not_count(self):
        with tempfile.TemporaryDirectory() as temp:
            bin_dir = pathlib.Path(temp)
            git = bin_dir / 'git'
            git.write_text('''#!/bin/sh
case "$*" in
  *'--abbrev-ref HEAD'*) echo main ;;
  *'rev-parse HEAD'*) echo abc123 ;;
esac
exit 0
''')
            git.chmod(0o755)
            gh = bin_dir / 'gh'
            gh.write_text('''#!/bin/sh
case "$*" in
  *'--workflow gate --event push'*)
    printf 'completed\\tfailure\\n' ;;
  *) printf 'completed\\tsuccess\\n' ;;
esac
''')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=temp + ':' + os.environ['PATH'])
            done = subprocess.run(['bash', str(SCRIPT)], env=env,
                                  capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0, done.stdout)
            self.assertNotIn('CI green', done.stdout)


if __name__ == '__main__':
    unittest.main()
