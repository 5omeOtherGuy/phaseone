#!/usr/bin/env python3
"""Unit tests for scripts/workflow.py — stdlib unittest, temp dirs only.

    python3 scripts/test_workflow.py [-q]

No p1 and no network: the Workflow is given a fake fanout (a Python script) that reads
the one-job batch, writes the output the test scripted for that label and attempt to the
OUTPUT FILE named in the brief (or the repair prompt), and prints a fanout-shaped summary.
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import workflow  # noqa: E402

FAKE_FANOUT = r'''
import json, os, re, sys
jobs = json.load(open(sys.argv[1]))
job = jobs[0]
text = open(job.get("prompt_file") or job["brief_file"]).read()
path = re.search(r"(?:OUTPUT FILE: |Your output file )(\S+)", text).group(1)
base = job["label"].split("-repair")[0]
script = json.load(open(os.environ["FAKE_SCRIPT"]))
calls_file = os.environ["FAKE_CALLS"]
calls = json.load(open(calls_file)) if os.path.exists(calls_file) else []
calls.append(job)
json.dump(calls, open(calls_file, "w"))
attempt = sum(1 for c in calls if c["label"].split("-repair")[0] == base) - 1
outputs = script.get(base, [])
if attempt < len(outputs) and outputs[attempt] is not None:
    open(path, "w").write(outputs[attempt])
run_dir = os.path.join(os.environ["FAKE_RUNS"], base)
os.makedirs(run_dir, exist_ok=True)
open(os.path.join(run_dir, "session.jsonl"), "a").close()
print(json.dumps([{"label": job["label"], "run_dir": run_dir, "outcome": "done",
                   "usage": None}]))
'''


def read(path):
    with open(path) as handle:
        return handle.read()


SCHEMA = {"type": "object", "additionalProperties": False, "required": ["answer"],
          "properties": {"answer": {"type": "string", "enum": ["yes", "no"]}}}


class SchemaTest(unittest.TestCase):
    def test_accepts_a_matching_value(self):
        self.assertEqual(workflow.schema_errors({"answer": "yes"}, SCHEMA), [])

    def test_names_every_problem(self):
        errors = workflow.schema_errors({"answer": "maybe", "extra": 1}, SCHEMA)
        self.assertEqual(len(errors), 2)
        self.assertTrue(any("maybe" in e for e in errors))
        self.assertTrue(any("extra" in e for e in errors))

    def test_nested_arrays_and_types(self):
        schema = {"type": "object", "properties": {"xs": {"type": "array", "minItems": 1,
                  "items": {"type": "integer"}}}, "required": ["xs"]}
        self.assertEqual(workflow.schema_errors({"xs": [1, 2]}, schema), [])
        self.assertIn("$.xs: needs at least 1 items", workflow.schema_errors({"xs": []}, schema))
        self.assertIn("$.xs[1]: expected integer, got bool",
                      workflow.schema_errors({"xs": [1, True]}, schema))


class WorkflowTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = self.tmp.name
        self.fake = os.path.join(root, "fake_fanout.py")
        with open(self.fake, "w") as handle:
            handle.write(FAKE_FANOUT)
        self.script = os.path.join(root, "script.json")
        self.calls = os.path.join(root, "calls.json")
        os.environ.update(FAKE_SCRIPT=self.script, FAKE_CALLS=self.calls,
                          FAKE_RUNS=os.path.join(root, "runs"))
        self.workspace = os.path.join(root, "ws")
        os.makedirs(self.workspace)
        self.out = os.path.join(root, "out")

    def tearDown(self):
        self.tmp.cleanup()

    def workflow(self, **kwargs):
        wf = workflow.Workflow(self.out, self.workspace, "deepseek2",
                               fanout=[sys.executable, self.fake], **kwargs)
        wf.log = lambda message: None
        return wf

    def scripted(self, outputs):
        with open(self.script, "w") as handle:
            json.dump(outputs, handle)

    def jobs(self):
        with open(self.calls) as handle:
            return json.load(handle)

    def test_a_valid_output_is_returned_and_the_brief_carries_the_contract(self):
        self.scripted({"a": ['{"answer": "yes"}']})
        wf = self.workflow()
        self.assertEqual(wf.agent("a", "Question?", SCHEMA), {"answer": "yes"})
        job = self.jobs()[0]
        self.assertEqual((job["runner"], job["env"], job["sandbox"]), ("p1", "deepseek2", True))
        self.assertIn(os.path.join(self.out, "outputs"), job["sandbox_write"])
        brief = read(job["brief_file"])
        self.assertTrue(brief.startswith("Question?"))
        self.assertIn(wf.output_path("a"), brief)
        self.assertIn('"enum"', brief)
        self.assertEqual(wf.failures, [])

    def test_an_invalid_output_gets_one_repair_round_in_the_same_session(self):
        self.scripted({"a": ['{"answer": "maybe"}', '{"answer": "no"}']})
        wf = self.workflow()
        self.assertEqual(wf.agent("a", "Q", SCHEMA), {"answer": "no"})
        first, repair = self.jobs()
        self.assertEqual(repair["label"], "a-repair1")
        self.assertTrue(repair["session"].endswith("session.jsonl"))
        self.assertIn("maybe", read(repair["prompt_file"]))
        self.assertNotIn("brief_file", repair)

    def test_a_still_invalid_output_is_a_recorded_failure_not_a_value(self):
        self.scripted({"a": ['{"answer": "maybe"}', "not json"]})
        wf = self.workflow()
        self.assertIsNone(wf.agent("a", "Q", SCHEMA))
        self.assertEqual(len(self.jobs()), 2)
        self.assertEqual(wf.failures[0]["label"], "a")
        self.assertEqual(wf.failures[0]["reason"], "invalid output")

    def test_a_missing_output_file_counts_as_invalid(self):
        self.scripted({"a": [None, None]})
        wf = self.workflow()
        self.assertIsNone(wf.agent("a", "Q", SCHEMA))
        self.assertIn("no output file was written", wf.failures[0]["errors"])

    def test_the_check_hook_rejects_like_the_schema(self):
        self.scripted({"a": ['{"answer": "no"}', '{"answer": "yes"}']})
        wf = self.workflow()
        check = lambda obj: [] if obj["answer"] == "yes" else ["answer must be yes"]
        self.assertEqual(wf.agent("a", "Q", SCHEMA, check=check), {"answer": "yes"})
        self.assertIn("answer must be yes", read(self.jobs()[1]["prompt_file"]))

    def test_after_run_voids_the_call(self):
        self.scripted({"a": ['{"answer": "yes"}']})
        wf = self.workflow()
        self.assertIsNone(wf.agent("a", "Q", SCHEMA, after_run=lambda label: "workspace dirty"))
        self.assertEqual(wf.failures[0]["reason"], "workspace dirty")

    def test_a_rerun_reuses_valid_outputs_without_a_job(self):
        self.scripted({"a": ['{"answer": "yes"}']})
        self.workflow().agent("a", "Q", SCHEMA)
        again = self.workflow()
        self.assertEqual(again.agent("a", "Q", SCHEMA), {"answer": "yes"})
        self.assertEqual(len(self.jobs()), 1)

    def test_duplicate_labels_are_a_definition_error(self):
        self.scripted({"a": ['{"answer": "yes"}']})
        wf = self.workflow()
        wf.agent("a", "Q", SCHEMA)
        with self.assertRaises(workflow.WorkflowError):
            wf.agent("a", "Q", SCHEMA)

    def test_pipeline_runs_items_independently_and_drops_none(self):
        wf = self.workflow()
        gate = threading.Event()
        order = []

        def first(value, item, index):
            if item == "slow":
                gate.wait(5)
            order.append(("first", item))
            return None if item == "drop" else value + "!"

        def second(value, item, index):
            order.append(("second", item))
            if item == "fast":
                gate.set()  # fast reaches stage 2 while slow is still in stage 1
            return (index, value)

        result = wf.pipeline(["slow", "fast", "drop"], first, second)
        self.assertEqual(result, [(0, "slow!"), (1, "fast!"), None])
        self.assertLess(order.index(("second", "fast")), order.index(("first", "slow")))

    def test_parallel_turns_an_exception_into_none_and_a_failure(self):
        wf = self.workflow()
        result = wf.parallel([lambda: 1, lambda: 1 / 0])
        self.assertEqual(result, [1, None])
        self.assertIn("ZeroDivisionError", wf.failures[0]["reason"])


if __name__ == "__main__":
    unittest.main()
