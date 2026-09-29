#!/usr/bin/env python3
"""CI sandbox provisioning refuses unusable bubblewrap in each test partition."""
import os
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


class CiBwrapTest(unittest.TestCase):
    def test_example_target_partition_is_present(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        self.assertIn('index("example")', workflow)
        self.assertIn('--example "$example"', workflow)

    def test_all_test_jobs_provision_and_failed_probe_fails(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        self.assertEqual(workflow.count('run: bash scripts/ci-bwrap.sh'), 4)
        with tempfile.TemporaryDirectory() as temp:
            bin_dir = pathlib.Path(temp)
            for name, body in [('sudo', 'exit 0'), ('bwrap', 'exit 73')]:
                path = bin_dir / name
                path.write_text('#!/bin/sh\n' + body + '\n')
                path.chmod(0o755)
            env = dict(os.environ, PATH=temp + ':' + os.environ['PATH'])
            done = subprocess.run(['bash', str(ROOT / 'scripts/ci-bwrap.sh')],
                                  env=env, capture_output=True, text=True)
            self.assertEqual(done.returncode, 73)

    def test_task_branch_build_provisions_and_rejects_the_skip_marker(self):
        # build.yml runs the same gate.sh, which permits unusable bubblewrap on CI; the
        # task-branch build must provision it and refuse a skipped boundary suite too.
        workflow = (ROOT / '.github/workflows/build.yml').read_text()
        self.assertIn('run: bash scripts/ci-bwrap.sh', workflow)
        self.assertIn("! grep -F 'SKIP: bwrap unusable here'", workflow)

    def test_workflows_reject_the_gates_explicit_unusable_bwrap_message(self):
        # gate.sh prints this to its own stdout, so it reaches the tee'd log; checking only the
        # per-test SKIP marker misses a skip because libtest captures a passing test's stderr
        # (Codex finding build.yml:92).
        guard = "! grep -F 'bubblewrap: unusable on this CI runner'"
        for name in ('ci.yml', 'build.yml'):
            with self.subTest(workflow=name):
                self.assertIn(guard, (ROOT / '.github/workflows' / name).read_text())

    def test_provisioning_probe_matches_the_gate_and_suite_probe(self):
        # A weaker provisioning probe can pass while the gate's probe fails; use the gate's own
        # invocation so passing here means the suites will run (Codex finding build.yml:92).
        script = (ROOT / 'scripts/ci-bwrap.sh').read_text()
        self.assertIn('bwrap --ro-bind / / --dev /dev --proc /proc true', script)

    def test_the_ci_sandbox_reversal_is_recorded_in_a_superseding_adr(self):
        # The provisioning reverses ADR-0077's GitHub-hosted skip rule, so a new proposed ADR
        # must supersede it (Codex finding ci.yml:66).
        adr_dir = ROOT / 'docs/adr'
        old = (adr_dir / '0077-builds-on-the-stream-boxes.md').read_text()
        self.assertIn('status: superseded', old)
        superseding = []
        for path in sorted(adr_dir.glob('[0-9][0-9][0-9][0-9]-*.md')):
            text = path.read_text()
            if 'supersedes: [77]' in text:
                superseding.append(path)
                self.assertIn('status: proposed', text)
        self.assertEqual(len(superseding), 1, [path.name for path in superseding])
        self.assertIn(f'superseded_by: [{int(superseding[0].name[:4])}]', old)


if __name__ == '__main__':
    unittest.main()
