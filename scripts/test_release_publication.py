#!/usr/bin/env python3
"""Exercise the release workflow's publish step without GitHub or credentials."""
import json
import os
import pathlib
import re
import subprocess
import tempfile
import unittest

WORKFLOW = pathlib.Path(__file__).resolve().parent.parent / '.github/workflows/release.yml'
# The one composition's assets (ADR-0217).
ASSETS = ('p1-linux-x86_64', 'p1-linux-x86_64.sha256',
          'p1-share.tar.gz', 'p1-share.tar.gz.sha256')


class ReleasePublicationTest(unittest.TestCase):
    def test_release_build_pins_an_exact_toolchain(self):
        # release-manifest.py embeds the rustc/cargo versions in p1-share.tar.gz, so the
        # byte-identical rerun comparison only holds when release builds use one exact
        # toolchain; the rolling `stable` ref would drift once stable advances.
        text = WORKFLOW.read_text()
        self.assertNotIn('dtolnay/rust-toolchain@stable', text)
        self.assertRegex(text, r'dtolnay/rust-toolchain@1\.\d+\.\d+')
        self.assertIn('targets: wasm32-unknown-unknown', text)

    def test_the_gate_workflows_pin_the_same_toolchain_as_release(self):
        # The required gate must compile with the same exact compiler as the release build:
        # when the gate tracks `stable` and only release is pinned, a PR can land syntax or
        # a dependency MSRV the pinned release compiler rejects, and the commit reaches main
        # but never publishes (Codex finding release.yml:39).
        root = WORKFLOW.parent
        pinned = re.search(r'dtolnay/rust-toolchain@(\d+\.\d+\.\d+)', WORKFLOW.read_text())
        self.assertIsNotNone(pinned)
        for name in ('ci.yml',):
            text = (root / name).read_text()
            # The gate's exact pin lives in its shared setup action; follow that
            # reference so moving setup does not drop the compiler parity check.
            self.assertIn('uses: ./.github/actions/gate-setup', text)
            text += (root.parent / 'actions/gate-setup/action.yml').read_text()
            with self.subTest(workflow=name):
                self.assertNotIn('dtolnay/rust-toolchain@stable', text)
                self.assertEqual(set(re.findall(r'dtolnay/rust-toolchain@(\S+)', text)),
                                 {pinned.group(1)})

    def test_complete_but_altered_asset_refuses_noop(self):
        script = step_script('Publish the release')
        script = script.replace('${{ steps.tag.outputs.tag }}', 'main-abc')
        script = script.replace('${{ needs.choose.outputs.sha }}', 'abc')
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
      *'--json assets'*) printf '%s\\n' "$ASSET_NAMES" ;;
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
                       GITHUB_REPOSITORY='fixture/repo', REMOTE=str(root / 'remote'),
                       ASSET_NAMES='\n'.join(ASSETS))
            done = subprocess.run(['bash', '-c', script], cwd=root, env=env,
                                  capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0, done.stdout)
            self.assertIn('differs', done.stderr)

    def test_prune_deletes_the_oldest_beyond_the_newest_30_at_most_100_a_run(self):
        # 140 releases listed out of date order, plus a tag outside the main- channel: the
        # run deletes the 100 oldest, never one of the newest 30 and never the other tag.
        releases = [{'tagName': f'main-{n:012x}', 'createdAt': f'2026-10-{1 + n // 24:02}T{n % 24:02}:00:00Z'}
                    for n in range(140)]
        listed = releases[70:] + [{'tagName': 'v0.1.0', 'createdAt': '2026-09-01T00:00:00Z'}] + releases[:70]
        with tempfile.TemporaryDirectory() as temp:
            root = pathlib.Path(temp)
            (root / 'bin').mkdir()
            (root / 'listed.json').write_text(json.dumps(listed))
            gh = root / 'bin' / 'gh'
            gh.write_text('''#!/bin/sh
case "$1:$2" in
  release:list)
    while [ "$#" -gt 0 ]; do
      if [ "$1" = --jq ]; then filter="$2"; shift 2; else shift; fi
    done
    jq -r "$filter" "$ROOT/listed.json" ;;
  release:delete) printf '%s\\n' "$3" >> "$ROOT/deleted" ;;
  *) exit 3 ;;
esac
''')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=str(root / 'bin') + ':' + os.environ['PATH'],
                       GITHUB_REPOSITORY='fixture/repo', ROOT=str(root))
            done = subprocess.run(['bash', '-c', step_script('Delete the releases beyond the newest 30')],
                                  cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(done.returncode, 0, done.stderr)
            deleted = (root / 'deleted').read_text().split()
            self.assertEqual(sorted(deleted), [r['tagName'] for r in releases[:100]])


def step_script(name):
    """The `run` script of the workflow step called `name`, unindented."""
    step = WORKFLOW.read_text().split(f'      - name: {name}\n', 1)[1]
    lines = []
    for line in step.split('        run: |\n', 1)[1].splitlines():
        if line.strip() and not line.startswith('          '):
            break
        lines.append(line[10:])
    return '\n'.join(lines)


if __name__ == '__main__':
    unittest.main()
