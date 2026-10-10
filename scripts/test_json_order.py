#!/usr/bin/env python3
"""Exercise the module feature guard with a scratch cargo, never a real build."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

GUARD = Path(__file__).resolve().parent / "check-json-order.sh"


class JsonOrderGuardTests(unittest.TestCase):
    def run_guard(self, tree, cargo_status=0):
        with tempfile.TemporaryDirectory() as scratch:
            cargo = Path(scratch) / "cargo"
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$TEST_TREE"\nexit "$TEST_STATUS"\n')
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=f"{scratch}:{os.environ['PATH']}",
                       TEST_TREE=tree, TEST_STATUS=str(cargo_status))
            return subprocess.run(["bash", str(GUARD)], env=env, text=True, capture_output=True)

    def test_sorted_maps_pass_but_preserve_order_fails(self):
        self.assertEqual(self.run_guard('serde_json feature "std"').returncode, 0)
        failed = self.run_guard('├── serde_json feature "preserve_order"')
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("a dependency enabled serde_json/preserve_order", failed.stderr)
        self.assertIn("issues/689", failed.stderr)

    def test_unrelated_feature_does_not_fail_and_cargo_failure_does(self):
        self.assertEqual(self.run_guard('other feature "preserve_order"').returncode, 0)
        self.assertNotEqual(self.run_guard("", cargo_status=2).returncode, 0)


if __name__ == "__main__":
    unittest.main()
