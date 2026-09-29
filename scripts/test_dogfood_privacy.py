#!/usr/bin/env python3
"""Dogfood run evidence is retained owner-only; the task prompt is never copied."""
import json
import os
import pathlib
import shlex
import subprocess
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / 'dogfood.sh'


class DogfoodPrivacyTest(unittest.TestCase):
    def test_private_streams_leave_inspectable_aggregate_without_prompt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            marker = 'private fixture prompt marker'
            report = root / 'report.json'
            report.write_text(json.dumps({
                'exit_code': 7, 'tool_calls': 2,
                'tool_calls_by_status': {'ok': 1, 'error': 1},
                'tool_calls_started_without_result': 0,
                'shell_exits': {'zero': 1, 'non_zero': 1},
                'private_message': marker,
            }))
            stdout = root / 'stdout.txt'
            stderr = root / 'stderr.txt'
            stdout.write_text(marker)
            stderr.write_text(marker + '\nerror\n')
            journal = root / 'session.jsonl'
            journal.write_text('\n'.join((
                json.dumps({'record': 'session_header', 'private_message': marker}),
                json.dumps({'record': 'tool_finished', 'result': {
                    'name': marker, 'status': 'error',
                    'content': marker + ' [exit code: 7]'}}),
            )) + '\n')
            diff = root / 'diff.numstat'
            diff.write_bytes(b'3\t1\t' + marker.encode() + b'\0')
            public = root / 'review-evidence.json'
            subprocess.run([sys.executable, str(SCRIPT.parent / 'dogfood-review.py'),
                            str(report), str(stdout), str(stderr), str(journal),
                            str(diff), str(public)],
                           check=True)
            text = public.read_text()
            self.assertNotIn(marker, text)
            record = json.loads(text)
            self.assertEqual(record['exit_code'], 7)
            self.assertEqual(record['tool_calls_by_status']['error'], 1)
            self.assertEqual(record['stderr_lines'], 2)
            self.assertEqual(record['stderr_error_lines'], 1)
            self.assertEqual(record['tool_results'], [
                {'index': 0, 'ok': False, 'exit_code': 7}])
            self.assertEqual((record['changed_files'], record['insertions'],
                              record['deletions']), (1, 3, 1))

    def test_rename_records_are_not_read_as_numstat_entries(self):
        # `git diff HEAD --numstat -z` emits a rename as `<add>\t<del>\t\0<old>\0<new>\0`;
        # reading the path tokens as numstat entries raised IndexError, so the run produced
        # no review-evidence.json at all (Codex finding dogfood-review.py:31).
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            report = root / 'report.json'
            report.write_text(json.dumps({'exit_code': 0, 'tool_calls': 0,
                                          'tool_calls_by_status': {},
                                          'tool_calls_started_without_result': 0,
                                          'shell_exits': {}}))
            empty = root / 'empty'
            empty.write_bytes(b'')
            diff = root / 'diff.numstat'
            diff.write_bytes(b'1\t0\tg.txt\0' + b'2\t1\t\0old.py\0new.py\0')
            public = root / 'review-evidence.json'
            subprocess.run([sys.executable, str(SCRIPT.parent / 'dogfood-review.py'),
                            str(report), str(empty), str(empty), str(empty),
                            str(diff), str(public)], check=True)
            record = json.loads(public.read_text())
            self.assertEqual((record['changed_files'], record['insertions'],
                              record['deletions']), (2, 3, 1))

    def test_raw_evidence_is_retained_owner_only_without_prompt(self):
        text = SCRIPT.read_text()
        # Codex finding dogfood.sh:43 (lead decision): the session journal, stdout and
        # stderr stay in the run directory so the independent review required by the
        # project AGENTS.md can inspect them.
        self.assertIn('"$run/session.jsonl"', text)
        self.assertIn('"$run/stdout.txt"', text)
        self.assertIn('"$run/stderr.txt"', text)
        self.assertNotIn('"$scratch/session.jsonl"', text)
        self.assertNotIn('"$scratch/stdout.txt"', text)
        self.assertNotIn('"$scratch/stderr.txt"', text)
        # The run directory is owner-only: 0700 directory, 0600 evidence files.
        self.assertIn('umask 077', text)
        self.assertIn('chmod 0700 "$run"', text)
        self.assertIn('chmod 0600 "$run/session.jsonl" "$run/stdout.txt" "$run/stderr.txt"', text)
        # The task prompt file itself is never copied into the retained directory.
        self.assertNotIn('cp "$task_file" "$run/task.txt"', text)
        self.assertNotIn('"$run/task.txt"', text)
        # Only the invocation scratch is removed on exit; the evidence is retained.
        self.assertIn("trap 'rm -rf -- \"$scratch\"' EXIT", text)

    def test_a_staged_deletion_is_counted(self):
        # `git add -A -N .` records a deletion in the index, so the numstat the run
        # collects must be taken against HEAD, not the index; otherwise a deletion-only
        # task reports zero changed files and zero deletions to the reviewer.
        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp) / 'clone'
            repo.mkdir()
            env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', HOME=str(repo))

            def git(*args: str) -> None:
                subprocess.run(
                    ['git', '-c', 'user.email=t@example.com', '-c', 'user.name=t',
                     '-c', 'commit.gpgsign=false', *args],
                    cwd=repo, env=env, check=True, capture_output=True)

            git('init', '-q')
            (repo / 'tracked.txt').write_text('one\n', encoding='utf-8')
            git('add', 'tracked.txt')
            git('commit', '-q', '-m', 'fixture')
            (repo / 'tracked.txt').unlink()
            git('add', '-A', '-N', '.')
            line = next(text for text in SCRIPT.read_text().splitlines()
                        if '--numstat' in text)
            self.assertIn('HEAD', line)
            scratch = pathlib.Path(tmp) / 'scratch'
            scratch.mkdir()
            script = (f'clone={shlex.quote(str(repo))}\n'
                      f'scratch={shlex.quote(str(scratch))}\n'
                      f'{line}\n'
                      f'cat "$scratch/diff.numstat"\n')
            done = subprocess.run(['bash', '-c', script], cwd=repo, env=env,
                                  capture_output=True, text=True)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(done.stdout, '0\t1\ttracked.txt\0')


if __name__ == '__main__':
    unittest.main()
