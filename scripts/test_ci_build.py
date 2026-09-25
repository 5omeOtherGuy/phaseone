#!/usr/bin/env python3
"""Unit tests for scripts/ci-build.sh — no network and no real git/gh/rust.

Every test copies the script into a temporary "repo" and puts stub `git`, `gh`,
`sleep` and `sha256sum` executables first on PATH. The stubs answer from
environment variables and log every call, so argument handling, the refusals, the
run identity and correlation, the artifact checks and the exit-code mapping can
all be checked without touching GitHub. Polling is driven by canned answer
sequences and call counters, never by elapsed time.

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
REAL_SHA256SUM = shutil.which("sha256sum") or "/usr/bin/sha256sum"

GIT_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    set -u
    echo "git $*" >> "$STUB_LOG"
    args="$*"
    if [ "${1:-}" = push ]; then
      exit "${STUB_PUSH_EXIT:-0}"
    fi
    if [ "${1:-}" = ls-remote ]; then
      if [ -n "${STUB_REMOTE_SHA:-}" ]; then
        printf '%s\\t%s\\n' "$STUB_REMOTE_SHA" "${4:-refs/heads/unknown}"
      fi
      exit "${STUB_LS_REMOTE_EXIT:-0}"
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

# Logs the invocation and delegates, so a test can prove the script ran
# `sha256sum -c` on the downloaded manifest, in the artifact directory.
SHA256SUM_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    echo "sha256sum $* (in $PWD)" >> "$STUB_LOG"
    exec %s "$@"
    """
    % REAL_SHA256SUM
)

# A tool that exists but fails, for the tooling-error exit-code tests.
FAILING_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    echo "{name} $*" >> "$STUB_LOG"
    echo "{name}: stub failure" >&2
    exit 1
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


    def next_answer(state_dir, name, default, counter):
        path = state_dir / counter
        count = int(path.read_text()) if path.exists() else 0
        path.write_text(str(count + 1))
        answers = env(name, default).split("|")
        return answers[min(count, len(answers) - 1)]


    def fail(message, code):
        print("gh stub: " + message, file=sys.stderr)
        sys.exit(code)


    state = pathlib.Path(os.environ["STUB_STATE"])

    if args[:2] == ["run", "list"]:
        code = int(env("STUB_LIST_EXIT", "0"))
        if code:
            fail("could not talk to the GitHub API", code)
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
        if "@tsv" not in query:
            # The pre-invocation snapshot: ids only, one per line.
            entry = next_answer(state, "STUB_IDS_OUT", "", "polls_ids")
            for one in entry.split():
                print(one)
            sys.exit(0)
        pinned = re.search(r'\\.databaseId==(\\d+)', query)
        if pinned:
            last = state / "last_id"
            if not last.exists() or pinned.group(1) != last.read_text().strip():
                sys.exit(0)  # pinned to a run this stub never reported
        excluded = set(re.findall(r'\\.databaseId != (\\d+)', query))
        entry = next_answer(
            state,
            "STUB_DISPATCH_OUT" if dispatched else "STUB_LIST_OUT",
            "in_progress\\t\\t7|completed\\tsuccess\\t7" if dispatched else "completed\\tsuccess\\t4242",
            "polls_dispatch" if dispatched else "polls",
        )
        if not entry:
            sys.exit(0)
        # A 4th field is the run's event; otherwise it is whatever the query asks for.
        fields = entry.split("\\t")
        event = fields[3] if len(fields) > 3 else (
            "push" if '.event=="push"' in query else "workflow_dispatch")
        if '.event=="push"' in query and event != "push":
            sys.exit(0)  # a run of another event, e.g. workflow_dispatch or pull_request
        if '.event=="workflow_dispatch"' in query and event != "workflow_dispatch":
            sys.exit(0)
        if len(fields) > 2 and fields[2] in excluded:
            sys.exit(0)  # a run that already existed before this invocation
        # The reuse query keeps only runs still queued or running, or already green.
        if '(.conclusion == "success")' in query and len(fields) > 1 and \\
                fields[0] == "completed" and fields[1] != "success":
            sys.exit(0)
        if len(fields) > 2:
            (state / "last_id").write_text(fields[2])
        print(entry)
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
        skip = set(env("STUB_DOWNLOAD_SKIP", "").split(","))
        if "p1" not in skip:
            (dest / "p1").write_bytes(P1_BYTES)
        if "p1.sha256" not in skip:
            sha = env("STUB_UPLOADED_SHA", hashlib.sha256(P1_BYTES).hexdigest())
            name = env("STUB_MANIFEST_NAME", "p1")
            (dest / "p1.sha256").write_text(sha + "  " + name + "\\n", encoding="utf-8")
        if "gate.log" not in skip:
            (dest / "gate.log").write_text("gate green\\n", encoding="utf-8")
        sys.exit(0)

    fail("unhandled call: " + " ".join(args), 2)
    """
    % (P1_BYTES,)
)


class Harness:
    """A temporary repo with the script and stub tools."""

    def __init__(
        self,
        fail_tools: dict[str, int] | None = None,
        tools: tuple[str, ...] | None = None,
        bare_path: bool = False,
        **env: str,
    ) -> None:
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
        stubs = {"git": GIT_STUB, "gh": GH_STUB, "sleep": SLEEP_STUB, "sha256sum": SHA256SUM_STUB}
        installed = set(tools) if tools is not None else set(stubs) | {"bash"}
        for name in sorted(installed):
            if name == "bash":
                os.symlink(shutil.which("bash") or "/usr/bin/bash", self.bin / "bash")
            elif name in stubs:
                self._stub(name, stubs[name])
            else:
                raise AssertionError(f"unknown stub {name}")
        for name in fail_tools or {}:
            self._stub(name, FAILING_STUB.format(name=name))
        path = str(self.bin) if bare_path else f"{self.bin}:{os.environ['PATH']}"
        self.env = {
            "PATH": path,
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
    def harness(self, **kwargs) -> Harness:
        harness = Harness(**kwargs)
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
        self.assertIn("verified with sha256sum -c", result.stdout)
        self.assertIn("gate [completed/success]", result.stdout)
        # Only build.yml's push run of this exact commit may be selected.
        query = [call for call in h.calls("gh") if "run list" in call and "@tsv" not in call]
        ts = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertTrue(ts, h.calls("gh"))
        self.assertIn("--workflow build.yml", ts[0])
        self.assertIn(f'event=="push"', ts[0])
        self.assertTrue(query, "the script snapshots the matching run ids before pushing")
        # The manifest is verified with sha256sum -c inside the artifact directory.
        checks = [call for call in h.calls("sha256sum") if "-c" in call]
        self.assertTrue(checks, h.calls("sha256sum"))
        self.assertIn(f"sha256sum -c p1.sha256 (in {h.artifact()})", checks[0])

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
                self.assertFalse(any("push" in call for call in h.calls("git")),
                                 "a refusal must not push")
                self.assertEqual(h.calls("gh"), [], "a refusal must not call gh")

    def test_detached_head_without_branch_is_a_usage_error(self) -> None:
        h = self.harness(**{"STUB_CURRENT_BRANCH": "HEAD"})
        result = h.run()
        self.assertEqual(result.returncode, 2)
        self.assertIn("detached HEAD", result.stderr)
        self.assertFalse(any("push" in call for call in h.calls("git")))
        self.assertEqual(h.calls("gh"), [])

    def test_detached_head_with_an_explicit_branch_is_refused(self) -> None:
        h = self.harness(**{"STUB_CURRENT_BRANCH": "HEAD"})
        result = h.run("task/safe")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("detached HEAD", result.stderr)
        self.assertEqual(h.calls("git"), ["git rev-parse --abbrev-ref HEAD"],
                         "nothing may be resolved or pushed from a detached HEAD")
        self.assertEqual(h.calls("gh"), [])

    def test_detached_head_on_wait_only_is_refused(self) -> None:
        h = self.harness(**{"STUB_CURRENT_BRANCH": "HEAD"})
        result = h.run("--wait-only")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("detached HEAD", result.stderr)
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
        self.assertFalse(any("download" in call or "workflow run" in call for call in h.calls("gh")),
                         "a failed push must not download or dispatch anything")

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
        polls = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertEqual(len(polls), 2, polls)
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
        self.assertIn("sha256 verification failed", result.stderr)
        self.assertIn("sha256 (downloaded) " + P1_SHA, result.stdout)
        self.assertIn("sha256 (uploaded)   " + "0" * 64, result.stdout)

    def test_manifest_for_another_file_exits_one(self) -> None:
        # sha256sum -c binds the digest to the file name in the manifest, so a
        # manifest that does not name p1 must fail verification (exit 1).
        h = self.harness(**{"STUB_MANIFEST_NAME": "not-p1"})
        result = h.run()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("sha256 verification failed", result.stderr)

    def test_artifact_without_the_binary_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_DOWNLOAD_EMPTY": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("artifact has no p1", result.stderr)

    def test_artifact_without_the_checksum_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_DOWNLOAD_SKIP": "p1.sha256"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("artifact has no p1.sha256", result.stderr)

    def test_artifact_without_the_gate_log_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_DOWNLOAD_SKIP": "gate.log"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("artifact has no gate.log", result.stderr)

    def test_failed_download_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_DOWNLOAD_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("could not download", result.stderr)

    def test_push_is_the_default_trigger(self) -> None:
        h = self.harness(**{"CI_TRIGGER": "push"})
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        polls = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertTrue(polls, h.calls("gh"))
        self.assertIn("--branch " + BRANCH, polls[0])
        self.assertIn("--workflow build.yml", polls[0])
        self.assertFalse(any("workflow run" in call for call in h.calls("gh")))

    def test_push_ignores_a_run_of_another_event(self) -> None:
        # A pull_request (or workflow_dispatch) run of the same commit is not the
        # push run, and the query says so.
        h = self.harness(
            **{"STUB_LIST_OUT": "completed\tsuccess\t7\tpull_request", "CI_BUILD_TIMEOUT": "0"}
        )
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("no completed run", result.stderr)
        polls = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertTrue(polls, h.calls("gh"))
        self.assertIn('event=="push"', polls[0])

    def test_push_accepts_only_a_run_it_caused(self) -> None:
        # Run 7 existed before the push; the push moved the branch, so only the new
        # run (9) may be accepted.
        h = self.harness(
            **{
                "STUB_IDS_OUT": "7",
                "STUB_LIST_OUT": "completed\tsuccess\t7|completed\tsuccess\t9",
                "STUB_VIEW_OUT": "build [completed/success] https://example.invalid/runs/9",
            }
        )
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        polls = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertIn("and .databaseId != 7", polls[0])
        views = [call for call in h.calls("gh") if call.startswith("gh run view 9 ")]
        self.assertTrue(views, h.calls("gh"))

    def test_push_that_moves_nothing_waits_for_that_commits_run(self) -> None:
        # Re-pushing the same commit creates no run at all, so the commit's own run
        # is the answer; no exclusion is applied.
        h = self.harness(
            **{
                "STUB_REMOTE_SHA": SHA,
                "STUB_IDS_OUT": "4242",
                "STUB_LIST_OUT": "completed\tsuccess\t4242",
            }
        )
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        polls = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertTrue(polls, h.calls("gh"))
        self.assertNotIn(".databaseId !=", polls[0])

    def test_ls_remote_failure_is_a_tooling_error(self) -> None:
        h = self.harness(**{"STUB_LS_REMOTE_EXIT": "1"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("git ls-remote origin", result.stderr)
        self.assertFalse(any("push" in call for call in h.calls("git")), "nothing may be pushed")

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
        polls = [call for call in lists if "@tsv" in call]
        self.assertEqual(len(polls), 3, polls)  # reuse check, then two polls
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
        self.assertIn("verified with sha256sum -c", result.stdout)

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

    def test_dispatch_accepts_only_the_run_it_started(self) -> None:
        # Run 7 matched the commit before the dispatch; after the dispatch only the
        # new run (9) may be accepted.
        h = self.harness(
            **{
                "CI_TRIGGER": "dispatch",
                "STUB_IDS_OUT": "7",
                "STUB_LIST_OUT": "",
                "STUB_DISPATCH_OUT": "completed\tsuccess\t7|completed\tsuccess\t9",
                "STUB_VIEW_OUT": "build [completed/success] https://example.invalid/runs/9",
            }
        )
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        polls = [call for call in h.calls("gh") if "run list" in call and "@tsv" in call]
        self.assertTrue(any("and .databaseId != 7" in call for call in polls), polls)
        self.assertTrue(any(call.startswith("gh run view 9 ") for call in h.calls("gh")), h.calls("gh"))

    def test_dispatch_wait_only_without_a_run_does_not_dispatch(self) -> None:
        h = self.harness(
            **{"CI_TRIGGER": "dispatch", "STUB_LIST_OUT": "", "CI_BUILD_TIMEOUT": "0"}
        )
        result = h.run("--wait-only")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("wait-only: no build.yml run", result.stdout)
        self.assertFalse(any("workflow run" in call for call in h.calls("gh")),
                         "wait-only must not start a run")
        self.assertFalse(any("push" in call for call in h.calls("git")))

    def test_tooling_failures_are_exit_two(self) -> None:
        # A local tool failure is a tooling error (2), never a red run (1).
        for tool in ("mktemp", "mkdir", "find", "mv", "sha256sum", "cut"):
            with self.subTest(tool=tool):
                h = self.harness(fail_tools={tool: 1})
                result = h.run()
                self.assertEqual(result.returncode, 2,
                                 f"{tool}: {result.stdout} {result.stderr}")

    def test_failing_sleep_is_a_tooling_error(self) -> None:
        h = self.harness(fail_tools={"sleep": 1}, **{"STUB_LIST_OUT": "in_progress\t\t7"})
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("sleep failed", result.stderr)

    def test_missing_git_is_a_tooling_error(self) -> None:
        # A PATH with only bash: the script must report the missing tool, not fail
        # with whatever `set -e` reports for the command that is not there.
        h = self.harness(tools=("bash",), bare_path=True)
        result = h.run()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("git is not on PATH", result.stderr)


if __name__ == "__main__":
    unittest.main()
