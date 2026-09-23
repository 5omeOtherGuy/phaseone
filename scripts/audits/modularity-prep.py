#!/usr/bin/env python3
"""Deterministic preparation for the modularity audit run as a p1 workflow.

    scripts/audits/modularity-prep.py [--repo DIR] [--sha COMMIT] [--only unit,unit] > args.json
    p1 workflow run scripts/audits/modularity.rhai --args args.json --workspace DIR

The rhai script (`modularity.rhai`) cannot read files or run programs, so everything the
Python definition (`modularity.py`) computed outside a model — the crate graph, the facts,
the unit list with its file budgets, the briefs — is computed here and handed to the script
as `args`. The script does what the Python `run` did with those inputs: one Find step per
unit, dedupe by evidence location, two refuters per finding and a third vote on a split
high/medium finding, then the confirmed/discarded buckets.

Not carried over (ADR-0053 "Deferred": required checks run by the host): the quote and repro
checks on a finder's output, the "claimed to read files it never opened" journal check, and
the critic rounds. The refuter briefs ask the refuter to run the repro itself.
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import modularity  # noqa: E402  (the Python definition is the source of every rule text)

REFUTE_PREAMBLE = """# Modularity audit — try to REFUTE one finding

Another auditor claims the finding below. Your job is to refute it. Answer `refuted`
if it is false, not a rule violation, already decided in an ADR, or if you are unsure.
Answer `upheld` only if you checked the code and the rule yourself and it holds.
Run the finding's `repro` command yourself and put its output (shortened) in `repro_output`.
Report your verdict as the structured `result` of `finish`: {"verdict", "reason", "repro_output"}.
"""

FIND_TRAILER = """
Report the structured `result` of `finish` as {"unit", "read", "findings"} — `read` lists every
file you opened, `findings` may be an empty list.
"""


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repo", default=os.getcwd())
    parser.add_argument("--sha", default=None)
    parser.add_argument("--only", default="", help="unit labels for a smoke run, comma-separated")
    args = parser.parse_args(argv)
    repo = os.path.abspath(args.repo)
    sha = args.sha or modularity.git(repo, "rev-parse", "HEAD").strip()
    scout = modularity.Scout(repo, sha)
    units = scout.units()
    problems = scout.validate(units)
    if problems:
        sys.exit("scout: " + "; ".join(problems))
    facts = scout.facts()
    units += [{"label": "errors", "lens": "error-ownership", "files": [], "index": scout.errors_index()},
              {"label": "test-seams", "lens": "test-seams", "files": [], "index": scout.tests_index()}]
    only = set(filter(None, args.only.split(",")))
    if only:
        units = [u for u in units if u["label"] in only]
        if len(units) != len(only):
            sys.exit(f"scout: unknown unit in only={sorted(only)}")
    prepared = [{"label": u["label"], "lens": u["lens"], "files": u["files"],
                 "brief": modularity.find_brief(u, facts) + FIND_TRAILER} for u in units]
    json.dump({
        "sha": sha,
        "facts": facts,
        "units": prepared,
        "findings_schema": modularity.FINDINGS_SCHEMA,
        "verdict_schema": modularity.VERDICT_SCHEMA,
        "refuter": modularity.REFUTER,
        "refute_preamble": REFUTE_PREAMBLE,
    }, sys.stdout, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
