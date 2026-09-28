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


if __name__ == '__main__':
    unittest.main()
