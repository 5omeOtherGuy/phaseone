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


WORKFLOW = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        ".github", "workflows", "ci.yml")


def run_block(after: str) -> str:
    """The dedented `run: |` script of the first step after `after` in ci.yml."""
    with open(WORKFLOW, encoding="utf-8") as handle:
        text = handle.read()
    lines = text.split(after, 1)[1].split("run: |\n", 1)[1].splitlines()
    indent = len(lines[0]) - len(lines[0].lstrip())
    block = []
    for line in lines:
        if line.strip() and len(line) - len(line.lstrip()) < indent:
            break
        block.append(line[indent:])
    return "\n".join(block) + "\n"


class ClassifyTest(unittest.TestCase):
    """ci.yml's `classify` step, run as written, with `gh` answering a fixed file list (#854)."""

    def classify(self, files: list[str], gh_status: int = 0,
                 event: str = "pull_request") -> dict[str, str]:
        listing = "".join(name + "\n" for name in files)
        with tempfile.TemporaryDirectory(prefix="classify-test-") as tmp:
            output = os.path.join(tmp, "output")
            env = {"PATH": "/usr/bin:/bin", "EVENT": event, "BASE": "1" * 40,
                   "HEAD": "2" * 40, "GITHUB_REPOSITORY": "o/r", "GITHUB_OUTPUT": output,
                   "FILES": listing, "GH_STATUS": str(gh_status),
                   # `gh` as an exported bash function: no stub file to write and execute.
                   "BASH_FUNC_gh%%": '() {  printf "%s" "$FILES"; return "$GH_STATUS"\n}'}
            done = subprocess.run(["bash", "-e", "-c", run_block("      - id: classify\n")],
                                  env=env, capture_output=True, text=True, check=False)
            self.assertEqual(done.returncode, 0, done.stderr)
            if gh_status == 0 and event == "pull_request":
                # The step read the list the API stand-in gave it.
                self.assertIn("changed files:\n" + listing.rstrip("\n"), done.stdout, done.stderr)
            with open(output, encoding="utf-8") as handle:
                return dict(line.split("=", 1) for line in handle.read().splitlines())

    def test_only_pack_files_is_pack_only(self) -> None:
        got = self.classify(["packs/owner/environments/gpt/environment.toml",
                             "packs/owner/environments/gpt/prompt.md",
                             "packs/owner/scripts/fanout.py"])
        self.assertEqual(got, {"docs_only": "false", "pack_only": "true"})

    def test_a_file_outside_packs_runs_the_full_gate(self) -> None:
        for outside in ("crates/p1-host/src/lib.rs", "environments/default/prompt.md",
                        "pack/owner/x.toml", "docs/packs/x.md"):
            with self.subTest(outside=outside):
                got = self.classify(["packs/owner/accounts/zai.toml", outside])
                self.assertEqual(got["pack_only"], "false")

    def test_a_rename_out_of_the_tree_is_not_pack_only(self) -> None:
        # The compare API lists a rename's old name too: moving a core file into a pack
        # changes the core.
        got = self.classify(["packs/owner/environments/default/prompt.md",
                             "environments/default/prompt.md"])
        self.assertEqual(got, {"docs_only": "false", "pack_only": "false"})

    def test_pack_markdown_outside_environments_is_also_docs_only(self) -> None:
        got = self.classify(["packs/owner/docs/STATUS.md"])
        self.assertEqual(got, {"docs_only": "true", "pack_only": "true"})

    def test_a_push_to_main_runs_the_full_gate_even_for_docs_or_packs(self) -> None:
        # ADR-0192: release.yml publishes the newest green push gate, so a docs-only or
        # pack-only main commit must not stand on a gate that skipped its Rust jobs.
        for files in (["README.md"], ["packs/owner/docs/notes.md"], ["packs/owner/x.toml"]):
            with self.subTest(files=files):
                self.assertEqual(self.classify(files, event="push"),
                                 {"docs_only": "false", "pack_only": "false"})

    def test_an_unreadable_or_oversized_change_runs_the_full_gate(self) -> None:
        self.assertEqual(self.classify(["TOO-MANY"]), {"docs_only": "false", "pack_only": "false"})
        self.assertEqual(self.classify(["packs/owner/x.toml"], gh_status=1),
                         {"docs_only": "false", "pack_only": "false"})
        self.assertEqual(self.classify([]), {"docs_only": "false", "pack_only": "false"})


if __name__ == "__main__":
    unittest.main()
