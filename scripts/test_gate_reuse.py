#!/usr/bin/env python3
"""Unit tests for scripts/gate_reuse.py: temp git repositories, no network.

    python3 scripts/test_gate_reuse.py [-q]
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gate_reuse  # noqa: E402


class GateReuseTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="gate-reuse-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.invalid")
        self.git("config", "user.name", "t")
        self.write("src/lib.rs", "fn a() {}\n")
        self.write("docs/adr/README.md", "index\nrow 1\n")
        self.commit("main start")
        self.git("checkout", "-q", "-b", "task")
        self.write("src/task.rs", "fn task() {}\n")
        self.write("docs/adr/README.md", "index\nrow 1\nrow task\n")
        self.green = self.commit("task change")

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", self.dir, *args], capture_output=True, text=True,
                              check=True).stdout.strip()

    def write(self, path: str, text: str) -> None:
        full = os.path.join(self.dir, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w", encoding="utf-8") as handle:
            handle.write(text)

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def on_main(self, path: str, text: str) -> None:
        self.git("checkout", "-q", "main")
        self.write(path, text)
        self.commit(f"main changes {path}")
        self.git("checkout", "-q", "task")

    def merge_main(self, resolve: dict[str, str] | None = None) -> str:
        done = subprocess.run(["git", "-C", self.dir, "merge", "-q", "--no-edit", "main"],
                              capture_output=True, text=True)
        if done.returncode != 0:
            for path, text in (resolve or {}).items():
                self.write(path, text)
            self.git("add", "-A")
            self.git("commit", "-q", "--no-edit")
        return self.git("rev-parse", "HEAD")

    def decide(self, greens: set[str] | None = None) -> tuple[bool, str]:
        greens = {self.green} if greens is None else greens
        return gate_reuse.decide(self.dir, "HEAD", "main", lambda sha: sha in greens)

    def test_a_clean_catch_up_with_main_reuses_the_green(self) -> None:
        self.on_main("src/other.rs", "fn other() {}\n")
        self.merge_main()
        reuse, reason = self.decide()
        self.assertTrue(reuse, reason)

    def test_a_markdown_conflict_resolved_by_hand_reuses_the_green(self) -> None:
        self.on_main("docs/adr/README.md", "index\nrow 1\nrow main\n")
        self.merge_main({"docs/adr/README.md": "index\nrow 1\nrow main\nrow task\n"})
        reuse, reason = self.decide()
        self.assertTrue(reuse, reason)

    def test_a_code_conflict_runs_the_full_gate(self) -> None:
        self.on_main("src/task.rs", "fn main_version() {}\n")
        self.merge_main({"src/task.rs": "fn task() {}\nfn main_version() {}\n"})
        reuse, reason = self.decide()
        self.assertFalse(reuse)
        self.assertIn("conflict in code: src/task.rs", reason)

    def test_a_code_edit_inside_a_clean_merge_runs_the_full_gate(self) -> None:
        self.on_main("src/other.rs", "fn other() {}\n")
        self.merge_main()
        self.write("src/lib.rs", "fn a() { changed() }\n")
        self.git("add", "-A")
        self.git("commit", "-q", "--amend", "--no-edit")
        reuse, reason = self.decide()
        self.assertFalse(reuse)
        self.assertIn("src/lib.rs", reason)

    def test_a_new_code_commit_after_the_green_runs_the_full_gate(self) -> None:
        self.write("src/task.rs", "fn task() { more() }\n")
        self.commit("more task work")
        self.assertFalse(self.decide()[0])

    def test_a_prompt_under_environments_is_code(self) -> None:
        self.write("environments/glm/prompt.md", "Be brief.\n")
        self.commit("prompt change")
        reuse, reason = self.decide()
        self.assertFalse(reuse)
        self.assertIn("environments/glm/prompt.md", reason)

    def test_a_prompt_in_a_pack_is_code(self) -> None:
        self.write("packs/owner/environments/glm/prompt.md", "Be brief.\n")
        self.commit("pack prompt change")
        reuse, reason = self.decide()
        self.assertFalse(reuse)
        self.assertIn("packs/owner/environments/glm/prompt.md", reason)

    def test_a_markdown_only_commit_after_the_green_reuses_it(self) -> None:
        self.write("docs/design/x.md", "notes\n")
        self.commit("docs")
        self.assertTrue(self.decide()[0])

    def test_without_an_earlier_green_the_full_gate_runs(self) -> None:
        self.on_main("src/other.rs", "fn other() {}\n")
        self.merge_main()
        reuse, reason = self.decide(greens=set())
        self.assertFalse(reuse)
        self.assertIn("no earlier commit", reason)

    def test_the_head_itself_never_counts_as_its_own_green(self) -> None:
        head = self.git("rev-parse", "HEAD")
        self.assertFalse(self.decide(greens={head})[0])


if __name__ == "__main__":
    unittest.main()
