#!/usr/bin/env python3
"""Unit tests for scripts/run-report.py — stdlib unittest, synthetic journals only.

    python3 scripts/test_run_report.py [-q]

No real p1, no network and no real session journal: every journal is written to a temp
directory by this file. `scripts/run-report.py` is the one that ships, imported by path
(importlib — the file name has a hyphen), so the numbers below come from the same code
path a real run uses. Every expected value is computed by hand from the fixture.
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

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "run-report.py")
_spec = importlib.util.spec_from_file_location("run_report", SCRIPT)
run_report = importlib.util.module_from_spec(_spec)
sys.modules["run_report"] = run_report
_spec.loader.exec_module(run_report)

ORIGIN = {"route": "test", "model": "test"}
# A `Usage` with every field unknown, i.e. what a report must say when nothing is known.
UNKNOWN_USAGE = {"input_uncached": None, "cache_read": None, "cache_write": None,
                 "output": None, "reasoning_output": None, "cost_micro_usd": None}
# The fields that existed before this change; every one of them must keep its value.
OLD_FIELDS = ("origin", "records", "requests", "interrupted_responses", "user_inputs",
              "inbox_messages", "context_replacements", "environment_records",
              "provider_retries", "tool_calls", "tool_calls_by_status", "tool_calls_not_ok",
              "tool_calls_by_tool", "tool_calls_started_without_result", "shell_exits",
              "usage", "input_total", "cache_read_share", "responses_without_usage",
              "stalled_on_summaries")


def environment(seq: int) -> dict:
    return {"seq": seq, "record": "environment", "route": {"origin": ORIGIN}}


def usage(input_uncached=None, cache_read=None, cache_write=None, output=None,
          reasoning_output=None, cost_micro_usd=None) -> dict:
    return {"input_uncached": input_uncached, "cache_read": cache_read,
            "cache_write": cache_write, "output": output,
            "reasoning_output": reasoning_output, "cost_micro_usd": cost_micro_usd}


def response(seq: int, usage_: dict) -> dict:
    return {"seq": seq, "record": "assistant_completed", "usage": usage_}


def replacement(seq: int, usage_: dict | None, with_key: bool = True) -> dict:
    record = {"seq": seq, "record": "context_replaced", "items": []}
    if with_key:
        record["usage"] = usage_
    return record


# Parent without replacements: two agent responses (one interrupted), one shell call.
# Response 1 leaves cache_write/reasoning_output/cost null; response 2 is fully known.
PARENT_RECORDS = [
    environment(0),
    {"seq": 1, "record": "user_input", "text": "do the thing"},
    response(2, usage(input_uncached=100, cache_read=300, output=40)),
    {"seq": 3, "record": "tool_started", "call_id": "c1",
     "identity": {"implementation": "p1-tool-shell", "variant": "plain"}},
    {"seq": 4, "record": "tool_finished",
     "result": {"call_id": "c1", "name": "shell", "status": "ok",
                "content": "did it\n[exit code: 0]"}},
    response(5, usage(input_uncached=50, cache_read=0, cache_write=20, output=10,
                      reasoning_output=5, cost_micro_usd=7)),
    {"seq": 6, "record": "assistant_interrupted"},
]
# 100+50, 300+0, 0+20 (null folded to 0), 40+10, none+5, none+7.
PARENT_USAGE = usage(input_uncached=150, cache_read=300, cache_write=20, output=50,
                     reasoning_output=5, cost_micro_usd=7)
# 150 + 300 + 20.
PARENT_INPUT_TOTAL = 470
PARENT_OLD = {
    "origin": ORIGIN,
    "records": 7,
    "requests": 3,                      # 2 completed + 1 interrupted
    "interrupted_responses": 1,
    "user_inputs": 1,
    "inbox_messages": 0,
    "context_replacements": 0,
    "environment_records": 1,
    "provider_retries": 0,
    "tool_calls": 1,
    "tool_calls_by_status": {"ok": 1},
    "tool_calls_not_ok": 0,
    "tool_calls_by_tool": {"shell": 1},
    "tool_calls_started_without_result": 0,
    "shell_exits": {"zero": 1, "non_zero": 0, "no_exit_code": 0},
    "usage": PARENT_USAGE,
    "input_total": PARENT_INPUT_TOTAL,
    "cache_read_share": 0.638,          # round(300 / 470, 3)
    "responses_without_usage": 0,
    "stalled_on_summaries": False,
}

WORKER_USAGE = usage(input_uncached=10, cache_read=0, cache_write=0, output=1)
WORKER_RECORDS = [environment(0), response(1, WORKER_USAGE)]

# Parent whose only worker start is a delegation tool call the host can read back.
DELEGATED_RECORDS = [
    environment(0),
    response(1, usage(input_uncached=10, cache_read=0, cache_write=0, output=1)),
    {"seq": 2, "record": "tool_started", "call_id": "c9",
     "identity": {"implementation": "p1-tool-delegate", "variant": "default"}},
    {"seq": 3, "record": "tool_finished",
     "result": {"call_id": "c9", "name": "worker_start", "status": "ok",
                "content": "Started worker w1 (a task)"}},
]


class RunReportTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="run-report-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    # --- helpers -----------------------------------------------------------

    def write_journal(self, name: str, records: list[dict]) -> str:
        """A minimal p1 journal: the header plus `records`, one JSON object per line."""
        path = os.path.join(self.dir, name)
        lines = [json.dumps({"p1_journal": 1})]
        lines.extend(json.dumps(record) for record in records)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")
        return path

    def analyze(self, path: str) -> dict:
        return run_report.analyze(path)

    def report(self, path: str) -> dict:
        return run_report.report(path)

    # --- old fields must not move -----------------------------------------

    def test_journal_without_replacements_keeps_every_old_field(self) -> None:
        path = self.write_journal("session.jsonl", PARENT_RECORDS)
        stats = self.analyze(path)
        self.assertEqual({key: stats[key] for key in OLD_FIELDS}, PARENT_OLD)
        report = self.report(path)
        self.assertEqual({key: report[key] for key in OLD_FIELDS}, PARENT_OLD)
        self.assertEqual(report["workers"], [])
        self.assertEqual(report["usage_with_workers"], PARENT_USAGE)
        self.assertEqual(report["input_total_with_workers"], PARENT_INPUT_TOTAL)
        self.assertFalse(report["includes_worker_usage"])

    # --- new fields --------------------------------------------------------

    def test_journal_without_replacements_has_unknown_summary_usage(self) -> None:
        path = self.write_journal("session.jsonl", PARENT_RECORDS)
        report = self.report(path)
        # Unknown, not zero: no replacement carried a usage.
        self.assertEqual(report["summary_usage"], UNKNOWN_USAGE)
        self.assertEqual(report["replacements_with_usage"], 0)
        self.assertEqual(report["usage_total"], PARENT_USAGE)
        self.assertEqual(report["input_total_all"], PARENT_INPUT_TOTAL)
        # Response 1 left cache_write null while carrying usage, so the input total is a
        # lower bound — even though no replacement is involved.
        self.assertIs(report["input_total_complete"], False)
        self.assertEqual(report["summary_usage_with_workers"], UNKNOWN_USAGE)
        self.assertEqual(report["usage_total_with_workers"], PARENT_USAGE)
        self.assertEqual(report["input_total_all_with_workers"], PARENT_INPUT_TOTAL)

    def test_replacement_usage_is_summed_apart_from_the_agent_usage(self) -> None:
        records = [
            environment(0),
            response(1, usage(input_uncached=100, cache_read=0, cache_write=0, output=1,
                              reasoning_output=0, cost_micro_usd=0)),
            replacement(2, usage(input_uncached=80, cache_read=20, cache_write=5, output=9,
                                 reasoning_output=3, cost_micro_usd=4)),
            replacement(3, usage(input_uncached=10, cache_read=0, cache_write=0, output=1)),
        ]
        report = self.report(self.write_journal("session.jsonl", records))
        self.assertEqual(report["context_replacements"], 2)
        self.assertEqual(report["replacements_with_usage"], 2)
        self.assertEqual(report["summary_usage"], usage(input_uncached=90, cache_read=20,
                                                        cache_write=5, output=10,
                                                        reasoning_output=3, cost_micro_usd=4))
        self.assertEqual(report["usage"], usage(input_uncached=100, cache_read=0, cache_write=0,
                                                output=1, reasoning_output=0, cost_micro_usd=0))
        self.assertEqual(report["input_total"], 100)
        self.assertEqual(report["usage_total"], usage(input_uncached=190, cache_read=20,
                                                      cache_write=5, output=11,
                                                      reasoning_output=3, cost_micro_usd=4))
        self.assertEqual(report["input_total_all"], 215)
        self.assertIs(report["input_total_complete"], True)
        # 190 + 20 + 5, over the parent's own summaries.
        self.assertEqual(report["summary_usage_with_workers"], report["summary_usage"])
        self.assertEqual(report["usage_total_with_workers"], report["usage_total"])
        self.assertEqual(report["input_total_all_with_workers"], 215)

    def test_replacement_without_usage_stays_null_not_zero(self) -> None:
        records = [
            environment(0),
            response(1, usage(input_uncached=100, cache_read=0, cache_write=0, output=1,
                              reasoning_output=0, cost_micro_usd=0)),
            replacement(2, None),            # explicit "usage": null
            replacement(3, None, with_key=False),   # a journal older than the field
        ]
        report = self.report(self.write_journal("session.jsonl", records))
        self.assertEqual(report["context_replacements"], 2)
        self.assertEqual(report["replacements_with_usage"], 0)
        self.assertEqual(report["summary_usage"], UNKNOWN_USAGE)
        self.assertEqual(report["usage_total"], report["usage"])
        self.assertEqual(report["input_total_all"], report["input_total"])
        self.assertEqual(report["input_total_complete"], True)

    def test_null_sub_field_in_a_replacement_makes_totals_incomplete(self) -> None:
        records = [
            environment(0),
            response(1, usage(input_uncached=100, cache_read=0, cache_write=0, output=1,
                              reasoning_output=0, cost_micro_usd=0)),
            replacement(2, usage(input_uncached=80, cache_read=0, cache_write=None, output=1)),
        ]
        report = self.report(self.write_journal("session.jsonl", records))
        self.assertIs(report["input_total_complete"], False)
        self.assertEqual(report["summary_usage"]["cache_write"], None)
        # cache_write folded to 0, and `input_total` itself is still the agent's alone.
        self.assertEqual(report["input_total_all"], 180)
        self.assertEqual(report["input_total"], 100)

    def test_no_usage_anywhere_is_null_not_zero(self) -> None:
        records = [
            environment(0),
            {"seq": 1, "record": "tool_started", "call_id": "c1",
             "identity": {"implementation": "p1-tool-shell", "variant": "plain"}},
            {"seq": 2, "record": "tool_finished",
             "result": {"call_id": "c1", "name": "shell", "status": "ok",
                        "content": "nothing\n[exit code: 0]"}},
            {"seq": 3, "record": "assistant_interrupted"},
        ]
        report = self.report(self.write_journal("session.jsonl", records))
        self.assertEqual(report["usage"], UNKNOWN_USAGE)
        self.assertEqual(report["summary_usage"], UNKNOWN_USAGE)
        self.assertEqual(report["usage_total"], UNKNOWN_USAGE)
        self.assertIsNone(report["input_total"])
        self.assertIsNone(report["input_total_all"])
        self.assertIsNone(report["input_total_complete"])
        self.assertIsNone(report["input_total_all_with_workers"])

    # --- workers -----------------------------------------------------------

    def test_parent_and_worker_file(self) -> None:
        path = self.write_journal("session.jsonl", PARENT_RECORDS)
        self.write_journal("session.jsonl.w1.jsonl", WORKER_RECORDS)
        report = self.report(path)
        self.assertEqual(report["workers"], [{
            "id": "w1",
            "origin": ORIGIN,
            "requests": 1,
            "tool_calls": 0,
            "usage": WORKER_USAGE,
            "input_total": 10,
            "summary_usage": UNKNOWN_USAGE,
            "replacements_with_usage": 0,
        }])
        self.assertEqual(report["usage_with_workers"],
                         usage(input_uncached=160, cache_read=300, cache_write=20, output=51,
                               reasoning_output=5, cost_micro_usd=7))
        self.assertEqual(report["input_total_with_workers"], 480)
        self.assertEqual(report["summary_usage_with_workers"], UNKNOWN_USAGE)
        self.assertEqual(report["usage_total_with_workers"], report["usage_with_workers"])
        self.assertEqual(report["input_total_all_with_workers"], 480)
        self.assertTrue(report["includes_worker_usage"])
        self.assertIs(report["worker_usage_known"], True)

    def test_worker_usage_known_is_false_when_a_delegated_worker_has_no_file(self) -> None:
        path = self.write_journal("session.jsonl", DELEGATED_RECORDS)
        report = self.report(path)
        self.assertEqual(report["workers_started"], ["w1"])
        self.assertFalse(report["includes_worker_usage"])
        self.assertIs(report["worker_usage_known"], False)
        # The journal does know the worker's tokens are missing, but not their value.
        self.assertEqual(report["usage"], usage(input_uncached=10, cache_read=0, cache_write=0,
                                                output=1))

    def test_worker_usage_known_is_true_when_the_worker_file_exists(self) -> None:
        path = self.write_journal("session.jsonl", DELEGATED_RECORDS)
        self.write_journal("session.jsonl.w1.jsonl", WORKER_RECORDS)
        report = self.report(path)
        self.assertTrue(report["includes_worker_usage"])
        self.assertIs(report["worker_usage_known"], True)
        self.assertEqual(report["usage_with_workers"]["input_uncached"], 20)

    def test_worker_usage_known_is_null_without_workers_or_delegation(self) -> None:
        path = self.write_journal("session.jsonl", PARENT_RECORDS)
        report = self.report(path)
        self.assertEqual(report["workers_started"], [])
        self.assertIsNone(report["worker_usage_known"])

    def test_a_failed_worker_start_is_not_a_delegated_worker(self) -> None:
        records = [
            environment(0),
            {"seq": 1, "record": "tool_started", "call_id": "c9",
             "identity": {"implementation": "p1-tool-delegate", "variant": "default"}},
            {"seq": 2, "record": "tool_finished",
             "result": {"call_id": "c9", "name": "worker_start", "status": "error",
                        "content": "no worker service"}},
        ]
        report = self.report(self.write_journal("session.jsonl", records))
        self.assertEqual(report["workers_started"], [])
        self.assertIsNone(report["worker_usage_known"])

    # --- the CLI -----------------------------------------------------------

    def test_cli_prints_the_new_fields(self) -> None:
        path = self.write_journal("session.jsonl", PARENT_RECORDS)
        self.write_journal("session.jsonl.w1.jsonl", WORKER_RECORDS)
        done = subprocess.run([sys.executable, SCRIPT, path, "--label", "t"],
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        record = json.loads(done.stdout)
        self.assertEqual(record["summary_usage"], UNKNOWN_USAGE)
        self.assertEqual(record["replacements_with_usage"], 0)
        self.assertEqual(record["input_total_all"], PARENT_INPUT_TOTAL)
        self.assertIs(record["input_total_complete"], False)
        self.assertIs(record["worker_usage_known"], True)
        self.assertEqual(record["label"], "t")


if __name__ == "__main__":
    unittest.main()
