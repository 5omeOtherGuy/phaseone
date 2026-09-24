#!/usr/bin/env python3
"""Unit tests for scripts/usage-audit.py — stdlib unittest, synthetic journals only.

    python3 scripts/test_usage_audit.py [-q]

No real run is read and no network is touched: every journal is written to a temp directory
by this file. `scripts/usage-audit.py` is the one that ships, imported by path (importlib —
the file name has a hyphen), so the numbers below come from the same code path a real
invocation uses. Every expected value is computed by hand from the fixture.
"""
from __future__ import annotations

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "usage-audit.py")
_spec = importlib.util.spec_from_file_location("usage_audit", SCRIPT)
usage_audit = importlib.util.module_from_spec(_spec)
sys.modules["usage_audit"] = usage_audit
_spec.loader.exec_module(usage_audit)

ROUTE = "openai-responses/codex-subscription"
MODEL = "gpt-6-sol"
# A `Usage` with every field unknown, i.e. what a request that reported nothing looks like.
NO_USAGE = {"input_uncached": None, "cache_read": None, "cache_write": None,
            "output": None, "reasoning_output": None, "cost_micro_usd": None}
# Long text that must never reach the output: a prompt, a message and a tool result.
SECRET = "SECRET-PROMPT-TEXT " + "x" * 5000


def usage(input_uncached=None, cache_read=None, cache_write=None, output=None,
          reasoning_output=None) -> dict:
    return {"input_uncached": input_uncached, "cache_read": cache_read,
            "cache_write": cache_write, "output": output,
            "reasoning_output": reasoning_output, "cost_micro_usd": None}


def environment(seq: int, route: str = ROUTE, model: str = MODEL) -> dict:
    return {"seq": seq, "record": "environment", "system_prompt": SECRET,
            "route": {"origin": {"route": route, "model": model}}}


def response(seq: int, usage_: dict | None) -> dict:
    return {"seq": seq, "record": "assistant_completed", "item": {"text": SECRET},
            "usage": usage_}


def replacement(seq: int, usage_: dict | None) -> dict:
    return {"seq": seq, "record": "context_replaced", "items": [{"text": SECRET}],
            "usage": usage_}


def strings(node, key=None):
    """Every string in a JSON document with the key it sits under."""
    if isinstance(node, dict):
        for name, value in node.items():
            yield from strings(value, name)
    elif isinstance(node, list):
        for value in node:
            yield from strings(value, key)
    elif isinstance(node, str):
        yield key, node


class UsageAuditTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="usage-audit-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    # --- helpers -----------------------------------------------------------

    def write_journal(self, relative: str, records: list, header: str = '{"p1_journal":1}',
                      raw_lines: list | None = None) -> str:
        """A journal file under the temp dir: header, then one JSON line per record (or the
        exact `raw_lines` when given)."""
        path = os.path.join(self.dir, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        lines = [header]
        if raw_lines is None:
            lines += [json.dumps(record) for record in records]
        else:
            lines += raw_lines
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")
        return path

    def write_text(self, relative: str, text: str) -> str:
        path = os.path.join(self.dir, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        done = subprocess.run([sys.executable, SCRIPT, *args], capture_output=True, text=True)
        return done.returncode, done.stdout, done.stderr

    def document(self, *args: str) -> dict:
        code, out, err = self.run_cli("--json", *args)
        self.assertEqual(code, 0, err)
        return json.loads(out)

    # --- (a) the worked formula case ---------------------------------------

    def test_worked_formula_case_is_exactly_point_eight(self) -> None:
        # read 80, uncached 10, write 10 -> 80 / (10 + 80 + 10) = 0.8.
        path = self.write_journal("session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10, output=5,
                              reasoning_output=2)),
        ])
        journal = usage_audit.analyze_journal(path)
        self.assertEqual(journal["cache_share"], 0.8)
        self.assertEqual(journal["cache_share_requests"], 1)
        self.assertEqual(journal["agent_requests"], 1)
        self.assertEqual(journal["summary_requests"], 0)
        self.assertEqual(journal["compactions"], 0)
        self.assertFalse(journal["partial"])       # every field of the one request is known
        self.assertEqual(journal["route"], ROUTE)
        self.assertEqual(journal["model"], MODEL)
        self.assertEqual(journal["tokens"], {"input_uncached": 10, "cache_read": 80,
                                             "cache_write": 10, "output": 5,
                                             "reasoning_output": 2})

        code, out, err = self.run_cli(path)
        self.assertEqual(code, 0, err)
        self.assertIn("| journal |", out)
        self.assertIn("80.0 %", out)
        self.assertEqual(self.document(path)["journals"][0]["cache_share"], 0.8)

    # --- (b) a null usage is unknown, not zero -----------------------------

    def test_null_usage_is_counted_unknown_and_leaves_the_share_alone(self) -> None:
        path = self.write_journal("session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10, output=5,
                              reasoning_output=1)),
            response(2, None),
        ])
        journal = usage_audit.analyze_journal(path)
        self.assertEqual(journal["agent_requests"], 2)
        self.assertEqual(journal["unknown_usage_requests"], 1)
        self.assertEqual(journal["unknown_fields"], {field: 1
                                                     for field in usage_audit.USAGE_FIELDS})
        self.assertTrue(journal["partial"])
        # The unknown request contributes nothing, so the share is the known one's.
        self.assertEqual(journal["cache_share"], 0.8)
        self.assertEqual(journal["cache_share_requests"], 1)
        self.assertEqual(journal["tokens"]["input_uncached"], 10)
        self.assertEqual(journal["tokens"]["output"], 5)
        self.assertFalse(journal["requests"][1]["usage_present"])

    # --- (c) one unknown field excludes the request from the share ---------

    def test_request_missing_only_cache_write_is_excluded_and_reported_partial(self) -> None:
        path = self.write_journal("session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10, output=5,
                              reasoning_output=1)),
            response(2, usage(input_uncached=1, cache_read=2, output=3, reasoning_output=2)),
        ])
        journal = usage_audit.analyze_journal(path)
        # Both requests are known in every field except the second's cache_write.
        self.assertEqual(journal["unknown_usage_requests"], 0)
        self.assertEqual(journal["unknown_fields"]["cache_write"], 1)
        self.assertEqual(journal["unknown_fields"]["input_uncached"], 0)
        self.assertTrue(journal["partial"])
        self.assertEqual(journal["cache_share"], 0.8)          # only the first qualifies
        self.assertEqual(journal["cache_share_requests"], 1)
        # The sums are over known values: the excluded field is unknown, the others add.
        self.assertEqual(journal["tokens"], {"input_uncached": 11, "cache_read": 82,
                                             "cache_write": 10, "output": 8,
                                             "reasoning_output": 3})

    # --- (d) an empty denominator is unknown, never a crash ----------------

    def test_zero_denominator_gives_no_share_and_no_zero_division(self) -> None:
        path = self.write_journal("session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=0, cache_read=0, cache_write=0, output=0,
                              reasoning_output=0)),
        ])
        journal = usage_audit.analyze_journal(path)
        self.assertIsNone(journal["cache_share"])
        self.assertEqual(journal["cache_share_requests"], 0)
        # Zero is a KNOWN count: the sums say 0, they do not say unknown.
        self.assertEqual(journal["tokens"]["input_uncached"], 0)
        self.assertEqual(journal["tokens"]["output"], 0)
        self.assertFalse(journal["partial"])
        document = self.document(path)
        self.assertIsNone(document["journals"][0]["cache_share"])
        self.assertIn("unknown", self.run_cli(path)[1])

    # --- (e) a compaction: summary apart, first request after it -----------

    def test_compaction_summary_is_separate_and_first_after_share_is_the_next_request(self) -> None:
        # a1: 10/80/10 out 5. summary: 1000/0/0 out 500 (a fresh prompt). a2: 20/40/40 out 7.
        path = self.write_journal("session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10, output=5,
                              reasoning_output=1)),
            replacement(2, usage(input_uncached=1000, cache_read=0, cache_write=0, output=500,
                                 reasoning_output=400)),
            response(3, usage(input_uncached=20, cache_read=40, cache_write=40, output=7,
                              reasoning_output=2)),
            response(4, usage(input_uncached=1, cache_read=1, cache_write=1, output=1,
                              reasoning_output=0)),
        ])
        journal = usage_audit.analyze_journal(path)
        self.assertEqual(journal["agent_requests"], 3)
        self.assertEqual(journal["summary_requests"], 1)
        self.assertEqual(journal["compactions"], 1)
        # The summarizer's tokens are NOT the agent's: no double counting.
        self.assertEqual(journal["tokens"], {"input_uncached": 31, "cache_read": 121,
                                             "cache_write": 51, "output": 13,
                                             "reasoning_output": 3})
        self.assertEqual(journal["summary_tokens"], {"input_uncached": 1000, "cache_read": 0,
                                                     "cache_write": 0, "output": 500,
                                                     "reasoning_output": 400})
        # The agent share is over the agent requests only: 121 / (31 + 121 + 51).
        self.assertEqual(journal["cache_share"], round(121 / 203, 4))
        self.assertEqual(journal["summary_cache_share"], 0.0)
        detail = journal["compaction_detail"][0]
        self.assertEqual(detail["summary"]["input_uncached"], 1000)
        self.assertEqual(detail["first_after"]["input_uncached"], 20)
        self.assertEqual(detail["first_after"]["cache_read"], 40)
        self.assertEqual(detail["first_after_share"], 0.4)     # 40 / (20 + 40 + 40)
        self.assertEqual(journal["requests"][1]["kind"], "summary")
        self.assertEqual(journal["requests"][2]["kind"], "agent")

    # --- (f) a worker journal is its own agent -----------------------------

    def test_worker_journal_is_a_separate_agent_and_not_in_the_parent(self) -> None:
        parent = self.write_journal("run-20260923-120000/session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10, output=5)),
        ])
        worker = self.write_journal("run-20260923-120000/session.jsonl.w1.jsonl", [
            environment(0, model="gpt-5.6-sol"),
            response(1, usage(input_uncached=1, cache_read=1, cache_write=1, output=1)),
        ])
        document = self.document(os.path.dirname(parent))
        self.assertEqual(document["totals"]["journals"], 2)
        self.assertEqual(document["totals"]["agent_requests"], 2)
        by_path = {journal["path"]: journal for journal in document["journals"]}
        self.assertEqual(by_path[parent]["agent"], "parent")
        self.assertIsNone(by_path[parent]["parent"])
        self.assertEqual(by_path[parent]["tokens"]["input_uncached"], 10)
        self.assertEqual(by_path[worker]["agent"], "w1")
        self.assertEqual(by_path[worker]["parent"], parent)
        self.assertEqual(by_path[worker]["tokens"]["input_uncached"], 1)
        self.assertEqual(document["cohort"][1]["agent"], "w1")
        self.assertEqual(document["cohort"][1]["records"], 2)

    # --- (g) malformed lines and non-journals ------------------------------

    def test_malformed_line_and_non_journal_are_skipped_with_a_reason(self) -> None:
        journal = self.write_journal("session.jsonl", [], raw_lines=[
            json.dumps(environment(0)),
            "{not json",
            json.dumps(response(1, usage(input_uncached=10, cache_read=80, cache_write=10))),
        ])
        not_a_journal = self.write_journal("session.jsonl.w1.jsonl", [environment(0)],
                                           header='{"p1_journal":2}')
        notes = self.write_text("notes.txt", "not a journal at all\n")
        code, out, err = self.run_cli("--json", self.dir, notes)
        self.assertEqual(code, 0, err)
        document = json.loads(out)
        self.assertEqual(document["totals"]["journals"], 1)      # the run is still reported
        self.assertEqual(document["journals"][0]["path"], journal)
        self.assertEqual(document["journals"][0]["malformed_lines"], 1)
        self.assertEqual(document["journals"][0]["agent_requests"], 1)
        self.assertEqual(document["journals"][0]["cache_share"], 0.8)
        skipped = {entry["path"]: entry["reason"] for entry in document["skipped"]}
        self.assertEqual(sorted(skipped), sorted([not_a_journal, notes]))
        for reason in skipped.values():
            self.assertIn("p1_journal", reason)
            self.assertLess(len(reason), 200)
        self.assertEqual(document["totals"]["skipped_files"], 2)

    def test_empty_directory_is_an_error_not_an_empty_report(self) -> None:
        code, out, err = self.run_cli(self.dir)
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertIn("no journals", err)

    def test_missing_path_is_skipped_with_a_reason_not_a_crash(self) -> None:
        journal = self.write_journal("session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10)),
        ])
        missing = os.path.join(self.dir, "gone.jsonl")
        code, out, err = self.run_cli("--json", journal, missing)
        self.assertEqual(code, 0, err)
        document = json.loads(out)
        self.assertEqual(document["totals"]["journals"], 1)
        self.assertEqual(document["totals"]["skipped_files"], 1)
        self.assertEqual(document["skipped"][0]["path"], missing)
        self.assertIn("cannot read", document["skipped"][0]["reason"])
        # The same file with a date filter: unreadable, still reported as skipped.
        code, out, err = self.run_cli("--json", "--since", "2020-01-01", missing)
        self.assertEqual(code, 1)
        self.assertIn("no journals", err)

    # --- (h) the JSON is safe to publish -----------------------------------

    def test_json_parses_and_carries_no_long_string_and_no_journal_text(self) -> None:
        self.write_journal("run-20260923-120000/session.jsonl", [
            environment(0),
            {"seq": 1, "record": "user_input", "text": SECRET},
            response(2, usage(input_uncached=10, cache_read=80, cache_write=10, output=5)),
            replacement(3, usage(input_uncached=1000, cache_read=0, cache_write=0, output=500)),
            response(4, None),
        ])
        self.write_text("run-20260923-120000/session.jsonl.w1.jsonl", "not a journal\n")
        code, out, err = self.run_cli("--json", self.dir)
        self.assertEqual(code, 0, err)
        document = json.loads(out)
        self.assertNotIn("SECRET", out)
        for key, value in strings(document):
            if key in ("path", "parent"):
                continue
            self.assertLessEqual(len(value), 200, f"{key}: {value[:80]!r}")

    # --- (i) --since and --label -------------------------------------------

    def test_since_and_label_select_the_cohort(self) -> None:
        old = self.write_journal("old-20260920-120000/session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=10, cache_read=80, cache_write=10)),
        ])
        new = self.write_journal("new-20260923-120000/session.jsonl", [
            environment(0),
            response(1, usage(input_uncached=1, cache_read=1, cache_write=1)),
        ])
        document = self.document("--since", "2026-09-23", self.dir)
        self.assertEqual([journal["path"] for journal in document["journals"]], [new])
        self.assertEqual(document["totals"]["filtered_out"], 1)
        document = self.document("--label", "old-", self.dir)
        self.assertEqual([journal["path"] for journal in document["journals"]], [old])
        document = self.document("--label", "new-", "--since", "2026-09-23", self.dir)
        self.assertEqual([journal["path"] for journal in document["journals"]], [new])
        self.assertEqual(document["filters"], {"since": "2026-09-23", "label": "new-"})

    def test_bad_since_is_rejected(self) -> None:
        code, _, err = self.run_cli("--since", "yesterday", self.dir)
        self.assertEqual(code, 2)
        self.assertIn("--since", err)


if __name__ == "__main__":
    unittest.main()
