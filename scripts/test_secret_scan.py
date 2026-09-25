#!/usr/bin/env python3
"""Unit tests for scripts/secret-scan.sh — stdlib unittest, temp git repos only.

    python3 scripts/test_secret_scan.py [-q]

Every key-shaped test vector is built at runtime (never a literal in this file), so
the test file itself never trips the scanner it is testing. Each case makes its own
temporary git repository, commits a fixture file, and runs the real
scripts/secret-scan.sh with that repo as the current working directory.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest

SCRIPTS = os.path.dirname(os.path.abspath(__file__))
SECRET_SCAN = os.path.join(SCRIPTS, "secret-scan.sh")
BASH = shutil.which("bash") or "/bin/bash"


class SecretScanTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="secret-scan-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def make_repo(self, filename: str, content: str) -> str:
        """A fresh git repo with one committed file; returns the repo directory."""
        repo = tempfile.mkdtemp(prefix="repo-", dir=self.dir)
        env = dict(os.environ)
        env["GIT_CONFIG_NOSYSTEM"] = "1"
        env["HOME"] = repo
        def run(*args: str) -> None:
            subprocess.run(
                ["git", "-c", "user.email=t@example.com", "-c", "user.name=t",
                 "-c", "commit.gpgsign=false", *args],
                cwd=repo, env=env, check=True, capture_output=True, text=True,
            )
        run("init", "-q")
        path = os.path.join(repo, filename)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(content)
        run("add", filename)
        run("commit", "-q", "-m", "fixture")
        return repo

    def scan(self, repo: str) -> subprocess.CompletedProcess:
        return subprocess.run([BASH, SECRET_SCAN], cwd=repo,
                              capture_output=True, text=True)

    def test_a_bare_sk_key_is_detected_and_never_printed(self) -> None:
        key = "sk-" + "a" * 24
        repo = self.make_repo("leaked.txt", key + "\n")
        done = self.scan(repo)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("leaked.txt:1", done.stdout)
        self.assertNotIn(key, done.stdout)
        self.assertNotIn(key, done.stderr)

    def test_a_clean_tree_passes(self) -> None:
        clean = "a" * 24
        repo = self.make_repo("clean.txt", "this is a normal sentence.\n" + clean + "\n")
        done = self.scan(repo)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertNotIn("clean.txt", done.stdout)

    def test_the_ant_family_prefix_is_detected(self) -> None:
        key = "sk-ant-" + "b" * 24
        repo = self.make_repo("leaked.txt", key + "\n")
        done = self.scan(repo)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("leaked.txt:1", done.stdout)
        self.assertNotIn(key, done.stdout)
        self.assertNotIn(key, done.stderr)

    def test_the_proj_family_prefix_is_detected(self) -> None:
        key = "sk-proj-" + "c" * 24
        repo = self.make_repo("leaked.txt", key + "\n")
        done = self.scan(repo)
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("leaked.txt:1", done.stdout)
        self.assertNotIn(key, done.stdout)
        self.assertNotIn(key, done.stderr)

    def test_a_too_short_bare_key_is_not_detected(self) -> None:
        # 19 characters after "sk-": one below the 20-character minimum.
        key = "sk-" + "a" * 19
        repo = self.make_repo("short.txt", key + "\n")
        done = self.scan(repo)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertNotIn("short.txt", done.stdout)


if __name__ == "__main__":
    unittest.main()
