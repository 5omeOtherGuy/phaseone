#!/usr/bin/env python3
"""Exercise the release workflow's publish step without GitHub or credentials."""
import os
import pathlib
import subprocess
import tempfile
import unittest

WORKFLOW = pathlib.Path(__file__).resolve().parent.parent / '.github/workflows/release.yml'
ASSETS = ('p1-linux-x86_64', 'p1-linux-x86_64.sha256',
          'p1-share.tar.gz', 'p1-share.tar.gz.sha256')


class ReleasePublicationTest(unittest.TestCase):
    def test_complete_but_altered_asset_refuses_noop(self):
        text = WORKFLOW.read_text()
        step = text.split('      - name: Publish the release\n', 1)[1]
        script = step.split('        run: |\n', 1)[1]
        script = '\n'.join(line[10:] for line in script.splitlines()
                           if line.startswith('          '))
        script = script.replace('${{ steps.tag.outputs.tag }}', 'main-abc')
        script = script.replace('${{ github.event.workflow_run.head_sha }}', 'abc')
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / 'dist').mkdir()
            (root / 'remote').mkdir()
            (root / 'bin').mkdir()
            for asset in ASSETS:
                (root / 'dist' / asset).write_bytes(b'original')
                (root / 'remote' / asset).write_bytes(b'original')
            (root / 'remote' / ASSETS[0]).write_bytes(b'altered')
            git = root / 'bin' / 'git'
            git.write_text('#!/bin/sh\nexit 0\n')
            git.chmod(0o755)
            gh = root / 'bin' / 'gh'
            gh.write_text('''#!/bin/sh
case "$2:$3" in
  view:*)
    case "$*" in
      *'--json assets'*) printf '%s\\n' p1-linux-x86_64 p1-linux-x86_64.sha256 p1-share.tar.gz p1-share.tar.gz.sha256 ;;
    esac ;;
  download:*)
    while [ "$#" -gt 0 ]; do
      if [ "$1" = --dir ]; then dest="$2"; shift 2; else shift; fi
    done
    cp "$REMOTE"/* "$dest"/ ;;
  *) exit 3 ;;
esac
''')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=str(root / 'bin') + ':' + os.environ['PATH'],
                       GITHUB_REPOSITORY='fixture/repo', REMOTE=str(root / 'remote'))
            done = subprocess.run(['bash', '-c', script], cwd=root, env=env,
                                  capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0, done.stdout)
            self.assertIn('differs', done.stderr)


if __name__ == '__main__':
    unittest.main()
