#!/usr/bin/env python3
"""Unit tests for scripts/fanout.py — stdlib unittest, temp dirs only.

    python3 scripts/test_fanout.py [-q]

No real p1 and no network: `P1_BIN` points at a fake shell script that records its
argv, writes a minimal valid p1 journal to `--session`, and exits with a code from
`FAKE_P1_EXIT`. `scripts/run-report.py` is the real one, so the summary numbers below
are produced by the same code path a real run uses.
"""
from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fanout  # noqa: E402

BRIEF = "do the thing"

# argv recorder + minimal journal + exit code from the environment.
FAKE_P1 = """#!/bin/sh
printf '%s\\n' "$0" "$@" > "$FAKE_P1_ARGV"
session=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--session" ]; then session="$arg"; fi
  prev="$arg"
done
mkdir -p "$(dirname "$session")"
{
  printf '%s\\n' '{"p1_journal":1}'
  printf '%s\\n' '{"seq":0,"record":"environment","route":{"origin":{"route":"fake","model":"fake"}}}'
  printf '%s\\n' '{"seq":1,"record":"assistant_completed","usage":{"input_uncached":100,"cache_read":300,"cache_write":null,"output":40,"reasoning_output":null,"cost_micro_usd":null}}'
} > "$session"
printf 'fake p1 output\\n'
printf 'fake p1 noise\\n' >&2
exit "${FAKE_P1_EXIT:-0}"
"""


class FanoutTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="fanout-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.work = os.path.join(self.dir, "work")
        os.makedirs(self.work)
        self.brief = self.write("brief.md", BRIEF)
        self.bin = self.write("p1", FAKE_P1)
        os.chmod(self.bin, 0o755)
        self.argv_log = os.path.join(self.dir, "argv.txt")
        self.set_env("P1_BIN", self.bin)
        self.set_env("FAKE_P1_ARGV", self.argv_log)
        self.set_env("FAKE_P1_EXIT", "0")
        self.set_attr("POLL_RUNNING_S", 0.02)
        self.set_attr("POLL_QUEUED_S", 0.02)

    # --- helpers -----------------------------------------------------------

    def write(self, name: str, text: str) -> str:
        path = os.path.join(self.dir, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def write_executable(self, subdir: str, name: str, text: str) -> str:
        """An executable inside `<self.dir>/<subdir>` — used as a PATH directory."""
        directory = os.path.join(self.dir, subdir)
        os.makedirs(directory, exist_ok=True)
        path = os.path.join(directory, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        os.chmod(path, 0o755)
        return path

    def read(self, path: str) -> str:
        with open(path, encoding="utf-8") as handle:
            return handle.read()

    def set_env(self, name: str, value: str) -> None:
        old = os.environ.get(name)
        os.environ[name] = value
        self.addCleanup(self.restore_env, name, old)

    @staticmethod
    def restore_env(name: str, old: str | None) -> None:
        if old is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = old

    def unset_env(self, name: str) -> None:
        old = os.environ.pop(name, None)
        self.addCleanup(self.restore_env, name, old)

    @staticmethod
    def path_without_p1() -> str:
        """The machine's PATH minus every directory that holds a `p1`."""
        return ":".join(
            directory
            for directory in os.environ.get("PATH", "").split(":")
            if not os.path.exists(os.path.join(directory, "p1"))
        )

    def set_attr(self, name: str, value) -> None:
        old = getattr(fanout, name)
        setattr(fanout, name, value)
        self.addCleanup(setattr, fanout, name, old)

    def p1_job(self, label: str = "p1-job", **overrides) -> dict:
        job = {"label": label, "runner": "p1", "env": "plain", "dir": self.work,
               "brief_file": self.brief}
        job.update(overrides)
        return job

    def run_jobs(self, jobs: list[dict]) -> tuple[int, str, str]:
        path = self.write("jobs.json", json.dumps(jobs))
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = fanout.main([path, "--max-parallel", "32", "--min-free-mb", "0"])
        return code, out.getvalue(), err.getvalue()

    def summary(self, jobs: list[dict]) -> list[dict]:
        code, out, err = self.run_jobs(jobs)
        self.assertIn(code, (0, 1), f"stderr: {err}")
        return json.loads(out)

    def recorded_argv(self) -> list[str]:
        with open(self.argv_log, encoding="utf-8") as handle:
            return handle.read().splitlines()

    def p1_argv(self, **overrides) -> tuple[list[str], dict]:
        job = self.p1_job(**overrides)
        entry = self.summary([job])[0]
        return self.recorded_argv(), entry

    # --- git worktrees -----------------------------------------------------

    def git(self, *args: str) -> None:
        env = dict(os.environ)
        env["GIT_CONFIG_GLOBAL"] = "/dev/null"
        env["GIT_CONFIG_SYSTEM"] = "/dev/null"
        done = subprocess.run(["git", *args], capture_output=True, text=True, env=env)
        self.assertEqual(done.returncode, 0, done.stderr)

    def make_repo(self, name: str) -> str:
        repo = os.path.join(self.dir, name)
        os.makedirs(repo)
        self.git("init", "-q", "-b", "main", repo)
        self.git("-C", repo, "-c", "user.email=t@t", "-c", "user.name=t",
                 "commit", "-q", "--allow-empty", "-m", "init")
        return repo

    def make_worktree(self, repo: str, name: str) -> str:
        worktree = os.path.join(self.dir, name)
        self.git("-C", repo, "worktree", "add", "-q", "--detach", worktree)
        return worktree

    def test_sandbox_read_paths_detects_a_worktree_and_ignores_a_clone(self) -> None:
        repo = self.make_repo("repo")
        worktree = self.make_worktree(repo, "wt")
        common = os.path.realpath(os.path.join(repo, ".git"))
        self.assertEqual(fanout.sandbox_read_paths(worktree), [common])
        self.assertEqual(fanout.sandbox_read_paths(repo), [])
        self.assertEqual(fanout.sandbox_read_paths(self.work), [])

    def test_worktree_job_passes_sandbox_read(self) -> None:
        repo = self.make_repo("repo")
        worktree = self.make_worktree(repo, "wt")
        argv, _ = self.p1_argv(dir=worktree, sandbox=True)
        common = os.path.realpath(os.path.join(repo, ".git"))
        self.assertEqual(argv.count("--sandbox-read"), 1, argv)
        self.assertEqual(argv[argv.index("--sandbox-read") + 1], common)
        # Before the brief and after the last writable path.
        self.assertLess(argv.index("--sandbox-read"), argv.index(BRIEF))
        self.assertGreater(argv.index("--sandbox-read"), argv.index("--sandbox-write"))

    def test_plain_clone_job_passes_no_sandbox_read(self) -> None:
        repo = self.make_repo("clone")
        argv, _ = self.p1_argv(dir=repo, sandbox=True)
        self.assertNotIn("--sandbox-read", argv)

    # --- argv --------------------------------------------------------------

    def test_default_argv_is_full_access(self) -> None:
        # Owner decision 2026-09-20 (ADR-0038): no sandbox unless the job asks for one,
        # not even for a worktree.
        repo = self.make_repo("repo-default")
        worktree = self.make_worktree(repo, "wt-default")
        argv, entry = self.p1_argv(dir=worktree, max_continuations=5)
        session = os.path.join(entry["run_dir"], "session.jsonl")
        self.assertEqual(argv, [
            self.bin, "--env", "plain", "--workspace", worktree, "--session", session,
            "--yes", "--max-continuations", "5", BRIEF,
        ])

    def test_model_key_reaches_p1_model(self) -> None:
        # Owner policy 2026-09-23: a job names the model inside its environment; without
        # the key the argv is unchanged (the environment's own profile runs).
        argv, entry = self.p1_argv(model="claude/claude-opus-5-5:high")
        session = os.path.join(entry["run_dir"], "session.jsonl")
        self.assertEqual(argv, [
            self.bin, "--env", "plain", "--workspace", self.work, "--session", session,
            "--model", "claude/claude-opus-5-5:high", "--yes", BRIEF,
        ])
        argv, _ = self.p1_argv()
        self.assertNotIn("--model", argv)

    def test_sandbox_write_without_sandbox_is_a_job_error(self) -> None:
        for job in (self.p1_job(sandbox_write=["/tmp/extra-write"]),
                    self.p1_job(sandbox="yes")):
            code, _, err = self.run_jobs([job])
            self.assertNotEqual(code, 0)
            self.assertIn("sandbox", err)

    def test_fresh_argv_is_exactly_specified(self) -> None:
        argv, entry = self.p1_argv(max_continuations=5, sandbox=True,
                                   sandbox_write=["/tmp/extra-write"])
        session = os.path.join(entry["run_dir"], "session.jsonl")
        self.assertEqual(argv, [
            self.bin, "--env", "plain", "--workspace", self.work, "--session", session,
            "--yes", "--sandbox", "workspace",
            "--sandbox-write", os.path.expanduser("~/.cargo/registry"),
            "--sandbox-write", os.path.expanduser("~/.cargo/git"),
            "--sandbox-write", f"/tmp/p1-build-locks-{os.getuid()}",
            "--sandbox-write", "/tmp/extra-write",
            "--max-continuations", "5",
            BRIEF,
        ])
        self.assertNotIn("--resume", argv)

    def test_resume_reuses_the_session_directory(self) -> None:
        first = self.summary([self.p1_job()])[0]
        run_dir = first["run_dir"]
        session = os.path.join(run_dir, "session.jsonl")
        self.assertIn("fake p1 output", self.read(os.path.join(run_dir, "stdout.txt")))
        repair = self.write("defects.md", "fix the defects")

        entry = self.summary([self.p1_job(label="p1-repair", session=session,
                                          prompt_file=repair)])[0]
        self.assertEqual(entry["run_dir"], run_dir)
        argv = self.recorded_argv()
        self.assertEqual(argv[argv.index("--session") + 1], session)
        self.assertEqual(argv[argv.index("--session") + 2], "--resume")
        self.assertEqual(argv[-1], "fix the defects")
        # The second run must not clobber the first run's evidence.
        self.assertIn("fake p1 output", self.read(os.path.join(run_dir, "stdout.txt")))
        self.assertIn("fake p1 output", self.read(os.path.join(run_dir, "stdout-2.txt")))
        self.assertTrue(os.path.isfile(os.path.join(run_dir, "stderr-2.txt")))
        self.assertEqual(first["stdout_file"], os.path.join(run_dir, "stdout.txt"))

    def test_run_dir_layout(self) -> None:
        entry = self.summary([self.p1_job(label="p1-layout")])[0]
        run_dir = entry["run_dir"]
        self.assertEqual(os.path.dirname(run_dir), os.path.join(self.dir, "runs"))
        self.assertRegex(os.path.basename(run_dir), r"^p1-layout-\d{8}-\d{6}$")
        for name in ("session.jsonl", "stdout.txt", "stderr.txt", "task.txt", "report.json"):
            self.assertTrue(os.path.isfile(os.path.join(run_dir, name)), name)
        self.assertEqual(self.read(os.path.join(run_dir, "task.txt")), BRIEF)

    # --- evidence ----------------------------------------------------------

    def test_report_json_and_summary_numbers(self) -> None:
        entry = self.summary([self.p1_job(label="p1-report", env="plain")])[0]
        report = json.loads(self.read(os.path.join(entry["run_dir"], "report.json")))
        self.assertEqual(report["requests"], 1)
        self.assertEqual(report["accepted"], "unknown")
        self.assertEqual(entry["runner"], "p1")
        self.assertEqual(entry["label"], "p1-report")
        self.assertEqual(entry["env"], "plain")
        self.assertEqual(entry["workspace"], self.work)
        self.assertEqual(entry["process_exit"], 0)
        self.assertEqual(entry["outcome"], "done")
        self.assertIsInstance(entry["wall_s"], int)
        self.assertEqual(entry["requests"], report["requests"])
        self.assertEqual(entry["tool_calls"], report["tool_calls"])
        self.assertEqual(entry["tool_calls_not_ok"], report["tool_calls_not_ok"])
        self.assertEqual(entry["shell_exits"], report["shell_exits"])
        self.assertEqual(entry["cache_read_share"], 0.75)
        self.assertEqual(entry["input_total_with_workers"], 400)
        self.assertEqual(entry["usage"]["output"], 40)
        with open(self.bin, "rb") as handle:
            self.assertEqual(report["binary_sha256"], hashlib.file_digest(handle, "sha256").hexdigest())

    def test_outcome_mapping(self) -> None:
        expected = {0: "done", 3: "blocked", 4: "stalled", 130: "cancelled", 1: "failed"}
        for code, word in expected.items():
            self.assertEqual(fanout.outcome_for_exit(code), word)
        for code, word in expected.items():
            os.environ["FAKE_P1_EXIT"] = str(code)
            entry = self.summary([self.p1_job(label=f"p1-exit-{code}")])[0]
            self.assertEqual(entry["outcome"], word, f"exit {code}")
            self.assertEqual(entry["process_exit"], code)

    # --- validation --------------------------------------------------------

    def assert_rejected(self, jobs: list[dict], needle: str) -> None:
        code, _, err = self.run_jobs(jobs)
        self.assertEqual(code, 1)
        self.assertIn(needle, err)

    def test_unknown_runner(self) -> None:
        job = self.p1_job()
        job["runner"] = "pi-agent"
        self.assert_rejected([job], "unknown runner")

    def test_session_without_prompt_file(self) -> None:
        session = self.write("session.jsonl", '{"p1_journal":1}\n')
        self.assert_rejected([self.p1_job(session=session)], "session requires prompt_file")

    def test_missing_brief_file(self) -> None:
        job = self.p1_job()
        job["brief_file"] = os.path.join(self.dir, "nope.md")
        self.assert_rejected([job], "brief file missing")

    def test_missing_workspace(self) -> None:
        self.assert_rejected([self.p1_job(dir=os.path.join(self.dir, "gone"))],
                             "no such workspace")

    def test_missing_p1_binary(self) -> None:
        os.environ["P1_BIN"] = os.path.join(self.dir, "no-p1")
        code, _, err = self.run_jobs([self.p1_job()])
        self.assertEqual(code, 1)
        self.assertIn("cargo build -p p1-host", err)

    # --- which p1 is run (ADR-0062) ---------------------------------------

    def test_p1_bin_wins_over_p1_on_path(self) -> None:
        on_path = self.write_executable("onpath", "p1", FAKE_P1)
        self.set_env("PATH", os.path.dirname(on_path))
        self.assertEqual(fanout.p1_binary(), self.bin)

    def test_p1_on_path_is_found_without_an_override(self) -> None:
        on_path = self.write_executable("onpath", "p1", FAKE_P1)
        self.unset_env("P1_BIN")
        self.set_env("PATH", os.path.dirname(on_path))
        self.assertEqual(fanout.p1_binary(), on_path)

    def test_a_job_with_no_override_runs_the_p1_on_path(self) -> None:
        on_path = self.write_executable("onpath", "p1", FAKE_P1)
        self.unset_env("P1_BIN")
        # The machine's own PATH (git, the interpreter) with every p1 removed, then the
        # stub in front: the only p1 this PATH names is the stub.
        self.set_env("PATH", os.path.dirname(on_path) + ":" + self.path_without_p1())
        argv, entry = self.p1_argv()
        self.assertEqual(argv[0], on_path)
        self.assertEqual(entry["outcome"], "done")

    def test_debug_fallback_is_used_when_nothing_else_exists(self) -> None:
        self.unset_env("P1_BIN")
        self.set_env("PATH", self.path_without_p1())
        debug = self.write_executable("sibling", "p1", FAKE_P1)
        self.set_attr("default_p1_binary", lambda: debug)
        self.assertEqual(fanout.p1_binary(), debug)

    def test_no_p1_anywhere_is_a_job_error_naming_the_fallback(self) -> None:
        self.unset_env("P1_BIN")
        self.set_env("PATH", self.path_without_p1())
        missing = os.path.join(self.dir, "no-p1")
        self.set_attr("default_p1_binary", lambda: missing)
        with self.assertRaises(fanout.JobError) as caught:
            fanout.p1_binary()
        self.assertIn(missing, str(caught.exception))
        self.assertIn("cargo build -p p1-host", str(caught.exception))

    def test_duplicate_labels(self) -> None:
        self.assert_rejected([self.p1_job(), self.p1_job()], "duplicate job labels")

    # --- pi-worker is untouched -------------------------------------------

    def test_pi_worker_command_is_unchanged(self) -> None:
        job = {"label": "w", "profile": "glm53", "effort": "high", "dir": "/work",
               "brief_file": "/brief.md"}
        self.assertEqual(fanout.command_for(job, "/usr/bin/pi-worker"),
                         ["/usr/bin/pi-worker", "glm53", "--dir", "/work",
                          "--effort", "high", "--brief-file", "/brief.md"])
        prompt = self.write("defects.md", "fix it")
        resume = {"label": "w2", "profile": "deepseek", "dir": "/work",
                  "session": "abc", "prompt_file": prompt}
        self.assertEqual(fanout.command_for(resume, "pi-worker"),
                         ["pi-worker", "deepseek", "--dir", "/work", "--effort", "high",
                          "--session", "abc", "--prompt", "fix it"])

    def test_live_worker_count_ignores_wrapper_processes(self) -> None:
        real = [["python3", "/home/u/brain-tools/scripts/pi-worker", "glm", "--dir", "/w"],
                ["python3", "/home/u/.local/bin/pi-worker", "sol6", "--dir", "/w"],
                ["pi-worker", "glm", "--dir", "/w"]]
        wrappers = [["bash", "-c", "for t in a b; do pi-worker glm --dir $t; done"],
                    ["python3", "/home/u/brain-tools/scripts/usage-meter", "wrap", "x", "--",
                     "/home/u/brain-tools/scripts/pi-worker", "sol6"],
                    ["/bin/bash", "-c", "source snapshot.sh && scripts/pi-worker glm --dir /w"],
                    ["python3", "scripts/fanout.py", "jobs.json"],
                    []]
        for argv in real:
            self.assertTrue(fanout.is_pi_worker(argv), argv)
        for argv in wrappers:
            self.assertFalse(fanout.is_pi_worker(argv), argv)
        self.assertTrue(fanout.is_p1_agent(["/x/phaseone-target/debug/p1", "--env", "deepseek2"]))
        self.assertFalse(fanout.is_p1_agent(["python3", "scripts/fanout.py", "--env"]))
        self.assertFalse(fanout.is_p1_agent(["p1", "models"]))


if __name__ == "__main__":
    unittest.main()
