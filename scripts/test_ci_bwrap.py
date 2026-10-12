#!/usr/bin/env python3
"""CI sandbox provisioning refuses unusable bubblewrap in each test partition."""
import os
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


class CiBwrapTest(unittest.TestCase):
    def test_caches_refresh_per_commit_and_do_not_restore_other_partitions(self):
        setup = (ROOT / '.github/actions/gate-setup/action.yml').read_text()
        save = (ROOT / '.github/actions/gate-cache-save/action.yml').read_text()
        key = "gate-v2-${{ runner.os }}-${{ inputs.partition }}-${{ hashFiles('Cargo.lock', 'modules/Cargo.lock') }}-${{ github.sha }}"
        self.assertIn('key: ' + key, setup)
        self.assertIn('key: ' + key, save)
        self.assertNotIn('cargo-${{ runner.os }}-\n', setup)
        self.assertIn('CARGO_INCREMENTAL: "0"', (ROOT / '.github/workflows/ci.yml').read_text())

    def test_example_target_partition_is_present(self):
        runner = (ROOT / 'scripts/ci-tests.py').read_text()
        # Cargo's default build compiles examples and executes examples with test=true.
        self.assertIn('target["test"]', runner)
        self.assertIn('"--no-run", "--message-format=json"', runner)

    def test_all_test_jobs_provision_and_failed_probe_fails(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        setup = (ROOT / '.github/actions/gate-setup/action.yml').read_text()
        self.assertIn('run: bash scripts/ci-bwrap.sh', setup)
        for job, following in (('checks', 'tests'), ('tests', 'gate')):
            block = workflow.split('\n  ' + job + ':', 1)[1].split('\n  ' + following + ':', 1)[0]
            self.assertIn('uses: ./.github/actions/gate-setup', block)
        # All four workspace shards and the one modules shard are required.
        self.assertIn('partition: [workspace]\n        shard: [1, 2, 3, 4]', workflow)
        self.assertIn('- partition: modules\n            shard: 1', workflow)
        self.assertIn("--shards \"${{ matrix.partition == 'workspace' && 4 || 1 }}\"", workflow)
        self.assertIn('&& matrix.shard == 1', workflow)
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
        runner = (ROOT / 'scripts/ci-tests.py').read_text()
        self.assertIn('"--no-default-features", "--test", "host", "without_delegation",\n'
                      '            "--", "without_tui"', runner)
        self.assertIn('if args.partition == "workspace" and args.shard == 1:', runner)
        # ADR-0214: the connectors' guest crates' unit tests run with the workspace partition.
        self.assertIn('"--manifest-path", "connectors/Cargo.toml",\n'
                      '            "-p", "p1-subscription-logins", "-p", "p1-subscription-usage"]',
                      runner)
        self.assertIn('needs: [classify, docs, pack, checks, tests]', workflow)
        self.assertIn('.tests.result == "success"', workflow)
        self.assertIn('fail-fast: false', workflow)

    def test_a_pack_only_change_runs_the_pack_on_the_released_p1(self):
        # #854: no toolchain and no build; the latest release checks the pack and runs its tests.
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        block = workflow.split('\n  pack:', 1)[1].split('\n  checks:', 1)[0]
        self.assertIn("if: needs.classify.outputs.pack_only == 'true'", block)
        self.assertNotIn('gate-setup', block)
        self.assertNotIn('cargo', block)
        self.assertIn('scripts/install.sh --from-release "$tag"', block)
        self.assertIn('env check --dir packs/owner', block)
        self.assertIn('packs/owner/tests/test_*.py packs/owner/scripts/test_*.py', block)
        self.assertIn('P1_BIN: ${{ runner.temp }}/p1/bin/p1', block)
        self.assertIn("steps.classify.outputs.pack_only == 'true'", workflow)

    def test_host_logs_reject_missing_runtime_directory_skips(self):
        workflow = (ROOT / '.github/workflows/ci.yml').read_text()
        for log in ('/tmp/p1-gate-checks.log',):
            guard = "! grep -F -e 'SKIP: bwrap unusable here' " \
                    "-e 'SKIP: no usable XDG_RUNTIME_DIR here' " + log
            self.assertIn(guard, workflow)
            for marker in ('SKIP: bwrap unusable here', 'SKIP: no usable XDG_RUNTIME_DIR here'):
                done = subprocess.run(['bash', '-c', guard.replace(log, '/dev/stdin')],
                                      input=marker + '\n', capture_output=True, text=True)
                self.assertNotEqual(done.returncode, 0)
        runner = (ROOT / 'scripts/ci-tests.py').read_text()
        for marker in ('SKIP: bwrap unusable here', 'SKIP: no usable XDG_RUNTIME_DIR here'):
            self.assertIn(marker, runner)
        self.assertIn('if any(marker in output for marker in SKIPS):', runner)

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
