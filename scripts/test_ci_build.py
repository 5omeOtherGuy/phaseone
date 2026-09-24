#!/usr/bin/env python3
"""Unit tests for scripts/ci-build.sh — no network and no real git/gh/rust.

Every test copies the script into a temporary "repo" and puts stub `git`, `gh` and
`sleep` executables first on PATH. The stubs answer from environment variables and
log every call, so argument handling, the refusals, the headSha match, the summary
and the exit-code mapping can all be checked without touching GitHub.

    python3 scripts/test_ci_build.py
"""
from __future__ import annotations

import hashlib
import os
import pathlib
import shutil
import subprocess
import tempfile
import textwrap
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
SCRIPT = REPO_ROOT / "scripts" / "ci-build.sh"

SHA = "1" * 40
OTHER_SHA = "2" * 40
BRANCH = "task/cloud-builds"
P1_BYTES = b"p1 debug binary\n"
P1_SHA = hashlib.sha256(P1_BYTES).hexdigest()

GIT_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    set -u
    echo "git $*" >> "$STUB_LOG"
    args="$*"
    if [ "${1:-}" = push ]; then
      exit "${STUB_PUSH_EXIT:-0}"
    fi
    if [ "$args" = "rev-parse --abbrev-ref HEAD" ]; then
      printf '%s\\n' "${STUB_CURRENT_BRANCH:-task/cloud-builds}"
      exit 0
    fi
    if [ "${1:-}" = rev-parse ] && [ "${2:-}" = --verify ]; then
      if [ "${4:-}" = "refs/heads/${STUB_BRANCH:-task/cloud-builds}" ]; then
        printf '%s\\n' "${STUB_SHA:-}"
        exit 0
      fi
      exit 1
    fi
    echo "git stub: unhandled $args" >&2
    exit 2
    """
)

SLEEP_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    echo "sleep $*" >> "$STUB_LOG"
    exit 0
    """
)

GH_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    "Stub gh: run list/view/download from environment variables."
    import hashlib
    import os
    import pathlib
    import re
    import sys

    P1_BYTES = %r
    args = sys.argv[1:]
    with open(os.environ["STUB_LOG"], "a", encoding="utf-8") as handle:
        handle.write("gh " + " ".join(args) + "\\n")


    def env(name, default=""):
        return os.environ.get(name, default)


    def fail(message, code):
        print("gh stub: " + message, file=sys.stderr)
        sys.exit(code)


    state = pathlib.Path(os.environ["STUB_STATE"])

    if args[:2] == ["run", "list"]:
        code = int(env("STUB_LIST_EXIT", "0"))
        if code:
            fail("could not talk to the GitHub API", code)
        # Emulates -q '.[] | select(.headSha=="<sha>") | [.status,.conclusion,.databaseId] | @tsv'
        query = args[args.index("-q") + 1] if "-q" in args else ""
        if "select(" not in query:
            fail("run list was not filtered", 2)
        # A workflow_dispatch run only exists once the script started one.
        dispatched = (state / "dispatched").exists()
        if 'event=="workflow_dispatch"' in query and not dispatched:
            sys.exit(0)
        wanted = re.search(r'\\.headSha=="([0-9a-f]+)"', query)
        if wanted and wanted.group(1) != env("STUB_RUN_SHA", env("STUB_SHA")):
            sys.exit(0)  # a run for a different commit does not match
        pinned = re.search(r'\\.databaseId==(\\d+)', query)
        if pinned:
            last = state / "last_id"
            if not last.exists() or pinned.group(1) != last.read_text().strip():
                sys.exit(0)  # pinned to a run this stub never reported
        polls = state / ("polls_dispatch" if dispatched else "polls")
        count = int(polls.read_text()) if polls.exists() else 0
        polls.write_text(str(count + 1))
        default = "in_progress\\t\\t7|completed\\tsuccess\\t7" if dispatched else "completed\\tsuccess\\t4242"
        answers = env("STUB_DISPATCH_OUT" if dispatched else "STUB_LIST_OUT", default).split("|")
        line = answers[min(count, len(answers) - 1)]
        if line:
            fields = line.split("\\t")
            # The reuse query keeps only runs that are still queued or running, or
            # already green: a red run for the commit is not reused.
            if '(.conclusion == "success")' in query and len(fields) > 1 and \\
                    fields[0] == "completed" and fields[1] != "success":
                sys.exit(0)
            if len(fields) > 2:
                (state / "last_id").write_text(fields[2])
            print(line)
        sys.exit(0)

    if args[:2] == ["workflow", "run"]:
        code = int(env("STUB_DISPATCH_EXIT", "0"))
        if code:
            fail("workflow has no workflow_dispatch trigger", code)
        (state / "dispatched").write_text("yes")
        sys.exit(0)

    if args[:2] == ["run", "view"] and "--json" in args:
        print(env("STUB_VIEW_OUT", "gate [completed/success] https://example.invalid/runs/4242"))
        sys.exit(0)

    if args[:2] == ["run", "view"] and "--log-failed" in args:
        code = int(env("STUB_LOG_FAILED_EXIT", "0"))
        if code:
            fail("no log for that run", code)
        print(env("STUB_LOG_FAILED", "error: dead code"))
        sys.exit(0)

    if args[:2] == ["run", "download"]:
        code = int(env("STUB_DOWNLOAD_EXIT", "0"))
        if code:
            fail("artifact not found", code)
        dest = pathlib.Path(args[args.index("--dir") + 1]) / env("STUB_ARTIFACT", "p1-build")
        dest.mkdir(parents=True, exist_ok=True)
        if env("STUB_DOWNLOAD_EMPTY", "0") == "1":
            sys.exit(0)
        (dest / "p1").write_bytes(P1_BYTES)
        sha = env("STUB_UPLOADED_SHA", hashlib.sha256(P1_BYTES).hexdigest())
        (dest / "p1.sha256").write_text(sha + "  p1\\n", encoding="utf-8")
        (dest / "gate.log").write_text("gate green\\n", encoding="utf-8")
        sys.exit(0)

    fail("unhandled call: " + " ".join(args), 2)
    """
    % (P1_BYTES,)
)


class Harness:
    """A temporary repo with the script and stub tools."""

    def __init__(self, **env: str) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="ci-build-test-")
        root = pathlib.Path(self.tmp.name)
        self.repo = root / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy(SCRIPT, self.repo / "scripts" / "ci-build.sh")
        os.chmod(self.repo / "scripts" / "ci-build.sh", 0o755)
        self.bin = root / "bin"
        self.bin.mkdir()
        self.log = root / "calls.log"
        self.state = root / "state"
        self.state.mkdir()
        self._stub("git", GIT_STUB)
        self._stub("gh", GH_STUB)
        self._stub("sleep", SLEEP_STUB)
        self.env = {
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HOME": str(root),
            "STUB_LOG": str(self.log),
            "STUB_STATE": str(self.state),
            "STUB_SHA": SHA,
        }
        self.env.update({key: str(value) for key, value in env.items()})

    def _stub(self, name: str, text: str) -> None:
        path = self.bin / name
        path.write_text(text, encoding="utf-8")
        os.chmod(path, 0o755)

    def run(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.repo / "scripts" / "ci-build.sh"), *args],
            cwd=self.repo,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )

    def calls(self, tool: str = "") -> list[str]:
        if not self.log.exists():
            return []
        lines = self.log.read_text(encoding="utf-8").splitlines()
        return [line for line in lines if not tool or line.startswith(tool + " ")]

    def artifact(self, sha: str = SHA) -> pathlib.Path:
        return self.repo / "ci-artifacts" / sha

    def cleanup(self) -> None:
        self.tmp.cleanup()


class CiBuildTests(unittest.TestCase):
    def harness(self, **env: str) -> Harness:
        harness = Harness(**env)
        self.addCleanup(harness.cleanup)
        return harness

    def test_green_run_pushes_downloads_and_verifies(self) -> None:
        h = self.harness()
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"git push --quiet origin refs/heads/{BRANCH}:refs/heads/{BRANCH}", h.calls("git"))
        download = h.calls("gh")
        self.assertTrue(any("run download" in call and "--name p1-build" in call for call in download), download)
        self.assertEqual((h.artifact() / "p1").read_bytes(), P1_BYTES)
        self.assertIn(P1_SHA, result.stdout)
        self.assertIn("sha256 (uploaded)", result.stdout)
        self.assertIn("verified against p1.sha256", result.stdout)
        self.assertIn("gate [completed/success]", result.stdout)

    def test_no_download_prints_summary_only(self) -> None:
        h = self.harness()
        result = h.run("--no-download")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("run download" in call for call in h.calls("gh")))
        self.assertFalse(h.artifact().exists())
        self.assertIn("gate [completed/success]", result.stdout)

    def test_wait_only_does_not_push(self) -> None:
        h = self.harness()
        result = h.run("--wait-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("push" in call for call in h.calls("git")), "wait-only must not push")
        self.assertTrue(any("run download" in call for call in h.calls("gh")))
        self.assertIn("wait-only", result.stdout)

    def test_named_branch_is_pushed(self) -> None:
        h = self.harness(**{"STUB_BRANCH": "task/other"})
        result = h.run("task/other")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("git push --quiet origin refs/heads/task/other:refs/heads/task/other", h.calls("git"))

    def test_refuses_main(self) -> None:
        for name in ("main", "refs/heads/main", "origin/main"):
            with self.subTest(branch=name):
                h = self.harness()
                result = h.run(name)
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("refusing to push", result.stderr)
                self.assertEqual(h.calls(), [], "a refusal must not touch git or gh")

    def test_detached_head_without_branch_is_a_usage_error(self) -> None:
        h = self.harness(**{"STUB_CURRENT_BRANCH": "HEAD"})
        result = h.run()
        self.assertEqual(result.returncode, 2)
        self.assertIn("detached HEAD", result.stderr)
        self.assertFalse(any("push" in call for call in h.calls("git")))
        self.assertEqual(h.calls("gh"), [])

    def test_unknown_option_is_a_usage_error(self) -> None:
        h = self.harness()
        result = h.run("--nope")
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown option", result.stderr)
        self.assertIn("usage:", result.stderr)
        self.assertEqual(h.calls(), [])

    def test_two_branches_is_a_usage_error(self) -> None:
        h = self.harness()
        result = h.run("task/a", "task/b")
        self.assertEqual(result.returncode, 2)
        self.assertIn("more than one branch", result.stderr)
        self.assertEqual(h.calls(), [])

    def test_help_succeeds(self) -> None:
        h = self.harness()
        result = h.run("--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("usage:", result.stderr)

    def test_unknown_local_branch_is_a_usage_error(self) -> None:
        h = self.harness()
        result = h.run("task/nope")
        self.assertEqual(result.returncode, 2)
        self.assertIn("no local branch task/nope", result.stderr)
        self.assertFalse(any("push" in call for call in h.calls("git")), "an unknown branch must not be pushed")
        self.assertEqual(h.calls("gh"), [])

    def test_failed_push_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_PUSH_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("git push origin", result.stderr)
        self.assertEqual(h.calls("gh"), [])

    def test_gh_run_list_failure_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_LIST_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("gh run list failed", result.stderr)

    def test_run_for_another_commit_is_not_accepted(self) -> None:
        h = self.harness(**{"STUB_RUN_SHA": OTHER_SHA, "CI_BUILD_TIMEOUT": "0"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("no completed run", result.stderr)
        self.assertIn("no run for", result.stdout)

    def test_polls_until_the_run_completes(self) -> None:
        h = self.harness(**{"STUB_LIST_OUT": "in_progress\t\t4242|completed\tsuccess\t4242"})
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        lists = [call for call in h.calls("gh") if "run list" in call]
        self.assertEqual(len(lists), 2, lists)
        self.assertTrue(h.calls("sleep"), "the poll loop must sleep between polls")

    def test_still_running_after_the_timeout_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_LIST_OUT": "in_progress\t\t4242", "CI_BUILD_TIMEOUT": "0"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("no completed run", result.stderr)
        self.assertIn("in_progress", result.stderr)
        self.assertEqual(h.calls("sleep"), [])

    def test_failed_run_exits_one_with_the_log_tail(self) -> None:
        h = self.harness(
            **{
                "STUB_LIST_OUT": "completed\tfailure\t4242",
                "STUB_LOG_FAILED": "error: this does not compile",
            }
        )
        result = h.run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("concluded 'failure'", result.stderr)
        self.assertIn("error: this does not compile", result.stderr)
        self.assertIn("gate [completed/success]", result.stdout)
        self.assertFalse(any("run download" in call for call in h.calls("gh")))

    def test_cancelled_run_exits_one(self) -> None:
        h = self.harness(**{"STUB_LIST_OUT": "completed\tcancelled\t4242"})
        result = h.run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("concluded 'cancelled'", result.stderr)

    def test_failed_run_without_a_log_still_exits_one(self) -> None:
        h = self.harness(**{"STUB_LIST_OUT": "completed\tfailure\t4242", "STUB_LOG_FAILED_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no failed-step log", result.stderr)

    def test_sha_mismatch_exits_one(self) -> None:
        h = self.harness(**{"STUB_UPLOADED_SHA": "0" * 64})
        result = h.run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("sha256 mismatch", result.stderr)
        self.assertIn("sha256 (downloaded) " + P1_SHA, result.stdout)
        self.assertIn("sha256 (uploaded)   " + "0" * 64, result.stdout)

    def test_artifact_without_the_binary_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_DOWNLOAD_EMPTY": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("has no p1 and p1.sha256", result.stderr)

    def test_failed_download_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_DOWNLOAD_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("could not download", result.stderr)

    def test_push_is_the_default_trigger(self) -> None:
        h = self.harness(**{"CI_TRIGGER": "push"})
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        lists = [call for call in h.calls("gh") if "run list" in call]
        self.assertTrue(lists, h.calls("gh"))
        self.assertIn("--branch " + BRANCH, lists[0])
        self.assertNotIn("--workflow", lists[0], "push mode looks the run up by branch")
        self.assertFalse(any("workflow run" in call for call in h.calls("gh")))

    def test_invalid_trigger_is_a_tooling_error(self) -> None:
        h = self.harness(**{"CI_TRIGGER": "webhook"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("CI_TRIGGER must be push or dispatch, not 'webhook'", result.stderr)
        self.assertEqual(h.calls(), [], "an invalid trigger must not touch git or gh")

    def test_dispatch_starts_a_run_and_waits_for_it(self) -> None:
        h = self.harness(**{"CI_TRIGGER": "dispatch", "STUB_LIST_OUT": ""})
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"gh workflow run build.yml --ref {BRANCH}", h.calls("gh"))
        self.assertIn("dispatch: started build.yml", result.stdout)
        lists = [call for call in h.calls("gh") if "run list" in call]
        self.assertTrue(any("--workflow build.yml" in call for call in lists), lists)
        self.assertTrue(any('event=="workflow_dispatch"' in call for call in lists), lists)
        self.assertEqual(len(lists), 3, lists)  # before dispatch, then two polls
        self.assertEqual((h.artifact() / "p1").read_bytes(), P1_BYTES)
        self.assertIn(P1_SHA, result.stdout)

    def test_dispatch_reuses_a_running_run(self) -> None:
        h = self.harness(
            **{
                "CI_TRIGGER": "dispatch",
                "STUB_LIST_OUT": "in_progress\t\t7|in_progress\t\t7|completed\tsuccess\t7",
            }
        )
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dispatch: reusing", result.stdout)
        self.assertFalse(any("workflow run" in call for call in h.calls("gh")), "must not start a second run")
        self.assertEqual((h.artifact() / "p1").read_bytes(), P1_BYTES)

    def test_dispatch_reuses_a_green_run(self) -> None:
        h = self.harness(**{"CI_TRIGGER": "dispatch", "STUB_LIST_OUT": "completed\tsuccess\t7"})
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dispatch: reusing", result.stdout)
        self.assertFalse(any("workflow run" in call for call in h.calls("gh")))
        self.assertIn("verified against p1.sha256", result.stdout)

    def test_dispatch_ignores_a_failed_run_for_the_same_commit(self) -> None:
        # The reuse query only matches queued/running/green runs, so a red run is
        # replaced by a fresh dispatch.
        h = self.harness(**{"CI_TRIGGER": "dispatch", "STUB_LIST_OUT": "completed\tfailure\t7"})
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"gh workflow run build.yml --ref {BRANCH}", h.calls("gh"))

    def test_dispatch_red_run_exits_one(self) -> None:
        h = self.harness(
            **{
                "CI_TRIGGER": "dispatch",
                "STUB_LIST_OUT": "",
                "STUB_DISPATCH_OUT": "completed\tfailure\t9",
                "STUB_LOG_FAILED": "error: the dispatch gate failed",
            }
        )
        result = h.run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("concluded 'failure'", result.stderr)
        self.assertIn("error: the dispatch gate failed", result.stderr)

    def test_dispatch_that_cannot_start_is_a_tooling_error(self) -> None:
        # p1's build.yml has no workflow_dispatch trigger: dispatch mode must fail
        # loudly instead of waiting for a run that can never exist.
        h = self.harness(**{"CI_TRIGGER": "dispatch", "STUB_LIST_OUT": "", "STUB_DISPATCH_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("gh workflow run build.yml --ref " + BRANCH + " failed", result.stderr)
        self.assertFalse(any("run download" in call for call in h.calls("gh")))

    def test_dispatch_wait_only_does_not_push(self) -> None:
        h = self.harness(**{"CI_TRIGGER": "dispatch", "STUB_LIST_OUT": "completed\tsuccess\t7"})
        result = h.run("--wait-only")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("push" in call for call in h.calls("git")), "wait-only must not push")
        self.assertIn("dispatch: reusing", result.stdout)


if __name__ == "__main__":
    unittest.main()
