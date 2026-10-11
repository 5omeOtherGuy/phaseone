#!/usr/bin/env python3
"""Coverage and failure controls for the CI test scheduler; no real Cargo build."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("ci_tests", Path(__file__).with_name("ci-tests.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def package(root, name, targets):
    return {"id": name, "name": name, "version": "1.2.3", "manifest_path": str(root / name / "Cargo.toml"),
            "targets": [{"name": target, "kind": [kind], "test": tested}
                        for target, kind, tested in targets]}


class CiTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.metadata = {
            "target_directory": str(self.root),
            "workspace_members": ["p1-module-tests", "new-package"],
            "packages": [package(self.root, "p1-module-tests", [("heavy", "test", True)]),
                         package(self.root, "new-package", [("new_package", "lib", True),
                             ("integration", "test", True), ("tested-example", "example", True),
                             ("compile-example", "example", False), ("binary", "bin", True)]),
                         package(self.root, "dependency", [("dep", "lib", True)])]}

    def messages(self, expected):
        return [{"reason": "compiler-artifact", "profile": {"test": True}, "package_id": key[0],
                 "target": {"name": key[1], "kind": list(key[2])},
                 "executable": str(self.root / key[1])} for key in expected]

    def test_partitions_cover_new_packages_and_every_test_target_once(self):
        workspace = runner.test_targets(self.metadata, "workspace")
        modules = runner.test_targets(self.metadata, "modules")
        self.assertFalse(set(workspace) & set(modules))
        self.assertEqual(len(workspace), 4)
        self.assertEqual(len(modules), 1)
        self.assertEqual(len(runner.executables(self.messages(workspace), workspace)), 4)
        self.assertEqual({key[1] for key in workspace},
                         {"new_package", "integration", "tested-example", "binary"})

    def test_missing_executable_fails_closed_instead_of_dropping_coverage(self):
        expected = runner.test_targets(self.metadata, "workspace")
        messages = self.messages(expected)
        with self.assertRaisesRegex(ValueError, "inventory mismatch"):
            runner.executables(messages[1:], expected)
        messages[0]["profile"]["test"] = False
        with self.assertRaisesRegex(ValueError, "inventory mismatch"):
            runner.executables(messages, expected)

    def test_binary_failure_and_successful_sandbox_skip_are_both_red(self):
        cwd = self.root / "new-package"
        cwd.mkdir()
        for body, expected in [("exit 7", 7), ("echo 'SKIP: bwrap unusable here'", 1),
                               ("echo 'SKIP: no usable XDG_RUNTIME_DIR here'", 1),
                               ('test "$PWD" = "$CARGO_MANIFEST_DIR" && '
                                'test "$CARGO_PKG_NAME" = new-package', 0)]:
            with self.subTest(body=body):
                binary = self.root / "binary"
                binary.write_text("#!/bin/sh\n" + body + "\n")
                binary.chmod(0o755)
                key = ("new-package", "binary", ("test",))
                _, env = runner.test_targets(self.metadata, "workspace")[("new-package", "binary", ("bin",))]
                result = runner.run_binary((key, binary, cwd, env), self.root, 5)
                self.assertEqual(result[1], expected)

    def test_all_binaries_run_concurrently_and_doctests_run_after_a_failure(self):
        expected = runner.test_targets(self.metadata, "workspace")
        barrier = threading.Barrier(4)
        seen = []

        def binary(target, logs, timeout):
            barrier.wait(timeout=5)  # Cannot pass if execution becomes serial.
            seen.append(target[0][1])
            status = int(target[0][1] == "integration")
            (logs / target[1].name).write_text("fake failing test" if status else "ok")
            return target[0][1], status, 0.0

        commands = []

        def cargo(command, **kwargs):
            commands.append(command)
            output = ("\n".join(json.dumps(x) for x in self.messages(expected))
                      if "--no-run" in command else "")
            return subprocess.CompletedProcess(command, 0, output)

        with patch.object(runner.subprocess, "check_output", return_value=json.dumps(self.metadata)), \
             patch.object(runner.subprocess, "run", side_effect=cargo), \
             patch.object(runner, "run_binary", side_effect=binary), \
             patch.dict(os.environ, {}, clear=False):
            self.assertEqual(runner.main(["--partition", "workspace"]), 1)
        self.assertEqual(set(seen), {key[1] for key in expected})
        self.assertTrue(any("--doc" in c for c in commands))
        self.assertTrue(any("without_delegation" in c for c in commands))

    def test_compile_failure_runs_no_executable_or_doctest(self):
        with patch.object(runner.subprocess, "check_output", return_value=json.dumps(self.metadata)), \
             patch.object(runner.subprocess, "run", return_value=subprocess.CompletedProcess([], 9, "")), \
             patch.object(runner, "run_binary") as binary:
            self.assertEqual(runner.main(["--partition", "workspace"]), 9)
            binary.assert_not_called()

    def test_a_hung_binary_times_out_red(self):
        binary = self.root / "hang"
        binary.write_text("#!/bin/sh\nwhile :; do :; done\n")
        binary.chmod(0o755)
        logs = self.root / "logs"
        logs.mkdir()
        target = (("pkg", "hang", ("test",)), binary, self.root, {})
        self.assertEqual(runner.run_binary(target, logs, 1)[1], 124)

    def test_doctest_failure_prevents_green_with_passing_binaries(self):
        expected = runner.test_targets(self.metadata, "modules")

        def cargo(command, **kwargs):
            return subprocess.CompletedProcess(command, 3 if "--doc" in command else 0,
                "\n".join(json.dumps(x) for x in self.messages(expected)) if "--no-run" in command else "")

        def binary(target, logs, timeout):
            (logs / target[1].name).write_text("test result: ok. 1 passed; 0 failed; 0 ignored;")
            return "heavy", 0, 0.0

        with patch.object(runner.subprocess, "check_output", return_value=json.dumps(self.metadata)), \
             patch.object(runner.subprocess, "run", side_effect=cargo), \
             patch.object(runner, "run_binary", side_effect=binary), \
             patch.dict(os.environ, {}, clear=False):
            self.assertEqual(runner.main(["--partition", "modules"]), 1)


if __name__ == "__main__":
    unittest.main()
