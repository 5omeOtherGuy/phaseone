#!/usr/bin/env python3
"""Dogfood run artifacts never retain raw task, journal or output files."""
import json
import pathlib
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

    def test_prompt_bearing_artifacts_are_temporary(self):
        text = SCRIPT.read_text()
        self.assertNotIn('cp "$task_file" "$run/task.txt"', text)
        self.assertNotIn('"$run/session.jsonl"', text)
        self.assertNotIn('"$run/stdout.txt"', text)
        self.assertNotIn('"$run/stderr.txt"', text)
        self.assertIn("trap 'rm -rf -- \"$scratch\"' EXIT", text)


if __name__ == '__main__':
    unittest.main()
