#!/usr/bin/env python3
"""Unit tests for scripts/journal-timing.py — stdlib unittest, synthetic journals only.

    python3 scripts/test_journal_timing.py [-q]

No real p1, no network and no real session journal: every journal is written to a
temp directory by this file. The example is ADR-0121's discriminating one, so the
expected figures are computed by hand from it, never from a run.
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

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "journal-timing.py")
_spec = importlib.util.spec_from_file_location("journal_timing", SCRIPT)
journal_timing = importlib.util.module_from_spec(_spec)
sys.modules["journal_timing"] = journal_timing
_spec.loader.exec_module(journal_timing)

# ADR-0121's example, one line per record, every line carrying the store clock's
# at_ms. The core read 1000 at send, 1250 at the first event (Activity), 1400 at
# the first output (TextDelta) and 1900 at Finished; a rate-limited wait of 2000
# ms arrived between send and that first event.
AT_MS = 5000
EXAMPLE_LINES = [
    {"p1_journal": 3},
    {"seq": 0, "record": "user_input", "text": "hi", "at_ms": AT_MS},
    {
        "seq": 1,
        "record": "assistant_completed",
        "item": {"origin": {"route": "test", "model": "test"}, "blocks": []},
        "stop": "end_turn",
        "usage": None,
        "at_ms": AT_MS,
    },
    {
        "seq": 2,
        "record": "request_timing",
        "request_index": 0,
        "sent_ms": 1000,
        "first_event_ms": 1250,
        "first_output_ms": 1400,
        "ended_ms": 1900,
        "waits": [{"reason": "rate_limited", "attempt": 1, "delay_ms": 2000}],
        "at_ms": AT_MS,
    },
]


class JournalTimingTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="journal-timing-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def write_journal(self, name: str, lines: list[dict]) -> str:
        path = os.path.join(self.dir, name)
        with open(path, "w", encoding="utf-8") as handle:
            for line in lines:
                handle.write(json.dumps(line) + "\n")
        return path

    # --- the ADR-0121 example ---------------------------------------------

    def test_the_adr_example_derives_its_four_figures(self) -> None:
        path = self.write_journal("example.jsonl", EXAMPLE_LINES)
        derived = journal_timing.analyze(path)
        self.assertEqual(
            derived,
            [
                {
                    "request_index": 0,
                    "time_to_first_event_ms": 250,   # 1250 - 1000
                    "time_to_first_output_ms": 400,  # 1400 - 1000
                    "decode_ms": 500,                # 1900 - 1400
                    "wait_count": 1,
                    "wait_total_ms": 2000,
                }
            ],
        )

    def test_the_cli_prints_the_example_figures(self) -> None:
        path = self.write_journal("example.jsonl", EXAMPLE_LINES)
        result = subprocess.run(
            [sys.executable, SCRIPT, path],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(
            result.stdout.strip(),
            "request 0: time to first event 250 ms, time to first output 400 ms, "
            "decode 500 ms, waits 1 (2000 ms)",
        )

    # --- edges -------------------------------------------------------------

    def test_a_request_without_output_reports_not_available(self) -> None:
        # A request that ended after the first event but produced no output at all.
        path = self.write_journal(
            "no-output.jsonl",
            [
                {"p1_journal": 3},
                {
                    "seq": 0,
                    "record": "request_timing",
                    "request_index": 1,
                    "sent_ms": 100,
                    "first_event_ms": 150,
                    "first_output_ms": None,
                    "ended_ms": 300,
                    "waits": [],
                    "at_ms": AT_MS,
                },
            ],
        )
        result = subprocess.run(
            [sys.executable, SCRIPT, path],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(
            result.stdout.strip(),
            "request 1: time to first event 50 ms, time to first output n/a, "
            "decode n/a, waits 0 (0 ms)",
        )

    def test_a_version_2_journal_reports_unknown_times(self) -> None:
        # A version-2 file carries no `at_ms` and no `request_timing`, so the script
        # has nothing to measure and says so (ADR-0121 point 5).
        path = self.write_journal(
            "v2.jsonl",
            [
                {"p1_journal": 2},
                {"seq": 0, "record": "user_input", "text": "hi"},
                {
                    "seq": 1,
                    "record": "assistant_completed",
                    "item": {"origin": {"route": "test", "model": "test"}, "blocks": []},
                    "stop": "end_turn",
                    "usage": None,
                },
            ],
        )
        result = subprocess.run(
            [sys.executable, SCRIPT, path],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertIn("unknown", result.stdout)

    def test_a_bad_invocation_reports_usage(self) -> None:
        result = subprocess.run(
            [sys.executable, SCRIPT],
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage: journal-timing.py", result.stderr)


if __name__ == "__main__":
    unittest.main()
