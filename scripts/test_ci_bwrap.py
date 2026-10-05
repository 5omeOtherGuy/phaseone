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

    def test_workflows_reject_the_gates_explicit_unusable_bwrap_message(self):
        # gate.sh prints this to its own stdout, so it reaches the tee'd log; checking only the
        # per-test SKIP marker misses a skip because libtest captures a passing test's stderr
        # (Codex finding build.yml:92).
        guard = "! grep -F 'bubblewrap: unusable on this CI runner'"
        for name in ('ci.yml',):
            with self.subTest(workflow=name):
                self.assertIn(guard, (ROOT / '.github/workflows' / name).read_text())

    def test_host_feature_off_acceptance_is_in_the_required_test_job(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        job = workflow.split('\n  test-workspace:', 1)[1].split('\n  gate:', 1)[0]
        self.assertIn('cargo test --locked --no-fail-fast -p p1-host '
                      '--no-default-features --test host without_delegation', job)
        self.assertIn('needs: [checks, test-modules-a, test-modules-b, test-workspace]', workflow)

    def test_host_logs_reject_missing_runtime_directory_skips(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        for log in ('/tmp/p1-gate-checks.log', '/tmp/p1-workspace-test.log'):
            guard = "! grep -F -e 'SKIP: bwrap unusable here' " \
                    "-e 'SKIP: no usable XDG_RUNTIME_DIR here' " + log
            self.assertIn(guard, workflow)
            for marker in ('SKIP: bwrap unusable here', 'SKIP: no usable XDG_RUNTIME_DIR here'):
                done = subprocess.run(['bash', '-c', guard.replace(log, '/dev/stdin')],
                                      input=marker + '\n', capture_output=True, text=True)
                self.assertNotEqual(done.returncode, 0)

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
                # Proposed until merged, accepted at the merge (owner 2026-10-01, #503); never
                # rejected or superseded while it carries the sandbox reversal.
                self.assertRegex(text, r'(?m)^status: (proposed|accepted)$')
        # A later ADR may supersede ADR-0077 too (ADR-0105 reverses its build placement); the
        # sandbox reversal stays pinned to ADR-0097 and ADR-0077 lists every superseding ADR.
        numbers = [int(path.name[:4]) for path in superseding]
        self.assertIn(97, numbers)
        self.assertIn(f'superseded_by: [{", ".join(str(n) for n in numbers)}]', old)

    def test_root_rule_matches_the_ci_sandbox_reversal(self):
        # Codex finding ci.yml:66: after ADR-0097 provisions bubblewrap on GitHub-hosted
        # runners, the root rule must not still claim CI lacks it, or workers and reviewers
        # expect the sandbox suites to skip there.
        agents = (ROOT / 'AGENTS.md').read_text()
        self.assertNotIn('CI lacks bubblewrap', agents)
        self.assertIn('CI provisions bubblewrap', agents)


if __name__ == '__main__':
    unittest.main()
