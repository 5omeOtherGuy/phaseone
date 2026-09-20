#!/usr/bin/env python3
"""Turn a p1 session journal into ONE run-level evidence record (JSON on stdout).

    scripts/run-report.py SESSION.jsonl [--label TEXT] [--elapsed SECONDS]
                          [--exit-code N] [--accepted yes|no|unknown] [--interventions N]
                          [--note TEXT] [--append docs/dogfood/runs.jsonl]

What the journal knows is counted; what only the operator knows (was the result accepted
after independent verification, how often a human had to step in) is passed in. Unknown
stays null — never zero. A non-zero shell exit is NOT a failed tool call (the tool ran and
reported it), so it is counted separately: a run with "0 failed tool calls" can still be full
of failing commands.

`requests` counts journalled agent responses (`assistant_completed` plus
`assistant_interrupted`) — never HTTP attempts, adapter transient retries, OAuth token
refreshes or the summarizer's own provider requests. HTTP attempts, retries and refreshes
leave no record in a journal at all; the summarizer's requests do — their tokens are on the
`context_replaced` record that caused them (see `summary_usage` below).

The host's own user-role messages are counted too: `provider_retries` counts the journalled
`PROVIDER_RETRY_MESSAGE` inputs (completion.md §3b), which are also part of `user_inputs`.

Summarization usage. A `context_replaced` record carries the `usage` of preparing the
replacement (1–2 real provider requests). It is summed apart from the agent's own usage,
because only the agent's usage describes what the model was asked to do:

    summary_usage            field-wise sum of every `context_replaced` usage; null while
                             nothing is known (no replacement, or replacements without usage)
    replacements_with_usage  how many replacements carried a usage
    usage_total              agent `usage` + `summary_usage`, field-wise; a field is null
                             only when both sides are null
    input_total_all          `input_total` computed over `usage_total`
    input_total_complete     false when one of the input sub-fields `input_total` adds
                             (`input_uncached`, `cache_read`, `cache_write`) was null in a
                             record that otherwise carried usage — that record could not say
                             what it input, so the total is a lower bound; true otherwise;
                             null when no record carried any usage. `input_total` itself keeps
                             its old meaning: the agent's own input, null sub-field folded to 0.

With `--session FILE`, each worker `w<N>` writes its own `FILE.w<N>.jsonl`; those files are
discovered automatically and reported under `workers`, with `usage_with_workers` and
`input_total_with_workers` adding them to the parent's numbers, and
`summary_usage_with_workers`, `usage_total_with_workers` and `input_total_all_with_workers`
doing the same for summarization usage. `includes_worker_usage` is true exactly when at
least one worker file was read (no worker file means no workers — a child session without
`--session` stays in memory and cannot be read back).

`worker_usage_known` answers the question `includes_worker_usage: false` leaves open:

    true   at least one worker journal file was read
    false  the parent journal shows a started worker — a `tool_finished` for a
           `p1-tool-delegate` call whose content begins "Started worker w<N>", the same
           evidence the host reads back — but no worker file exists, so that worker ran
           without `--session` and its tokens are missing from every total
    null   the parent journal shows no worker at all

`workers_started` lists the worker ids (`w1`, …) the parent journal recorded, in order —
the evidence `worker_usage_known` reads. A worker started without `--session` is listed
here and nowhere else.

Which build ran. A journal does not know the binary that wrote it, so the caller says:
`harness_head` (the p1 repository's revision when the run started — a hint, the binary may be
older) and `binary_sha256` (the hash of the p1 binary — two runs with the same hash ran the same
build). Both are null when not passed; never guessed.

`stalled_on_summaries` is derived from the journal (completion.md §3c): the longest run of
`context_replaced` records after the last `tool_finished` that was a mutating tool (`write`,
`edit`, `apply_patch`) or `finish`, AND the journal's last response is an interrupted one.
A run that completed ends on `assistant_completed`; a stalled run is cancelled mid-turn and
ends on `assistant_interrupted`. The bound is the host's `--max-idle-summaries` default of 6
unless `--max-idle-summaries` is passed to this script.
"""
import argparse
import glob
import json
import re
import sys

EXIT_CODE = re.compile(r"\[exit code: (-?\d+)\]\s*$")
WORKER_FILE = re.compile(r"\.w(\d+)\.jsonl$")
SHELL_IMPLEMENTATION = "p1-tool-shell"
# The host's ONE retry message after a transient provider failure (completion.md
# §3b). A `user_input` with exactly this text is a provider retry.
PROVIDER_RETRY_MESSAGE = ("The connection to the model failed and the last response was lost; "
                          "nothing else changed. Continue the work now.")
# completion.md §3c: the default bound on consecutive context replacements without
# progress. The report cannot see the CLI flag, so it uses the host's default unless
# the caller passes --max-idle-summaries.
DEFAULT_MAX_IDLE_SUMMARIES = 6
# A mutating tool result is progress; `finish` is progress whatever its status. The
# host decides by durable effect; a journal only has the model-facing name.
MUTATING_TOOLS = frozenset(("write", "edit", "apply_patch"))
FINISH_TOOL = "finish"
# `ToolStarted.identity.implementation` of the delegation tools (their crate name); a
# successful `worker_start` result starts with the prefix and names the worker's id.
DELEGATION_IMPLEMENTATION = "p1-tool-delegate"
WORKER_STARTED_PREFIX = "Started worker "
# Every field of a `Usage` the report sums.
USAGE_FIELDS = ("input_uncached", "cache_read", "cache_write", "output", "reasoning_output",
                "cost_micro_usd")
# The sub-fields `input_total` adds; a null one makes the total a lower bound.
INPUT_FIELDS = ("input_uncached", "cache_read", "cache_write")


def add(total, value):
    """Sum of known values; None while nothing is known."""
    if value is None:
        return total
    return value if total is None else total + value


def usage_sum(*usages):
    """Field-wise sum of usage dicts; a field is null only when it is null in all of them."""
    total = {key: None for key in USAGE_FIELDS}
    for one in usages:
        for key in total:
            total[key] = add(total[key], one[key])
    return total


def input_total_of(usage):
    """`input_total` over one usage dict: null while input_uncached is unknown, otherwise
    the input counts, a null sub-field folded to 0."""
    if usage["input_uncached"] is None:
        return None
    return usage["input_uncached"] + (usage["cache_read"] or 0) + (usage["cache_write"] or 0)


def analyze(path, max_idle_summaries=DEFAULT_MAX_IDLE_SUMMARIES):
    """Every metric of ONE journal file (the parent's, or one worker's)."""
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    header = json.loads(lines[0])
    if header.get("p1_journal") != 1:
        sys.exit(f"{path}: not a p1 journal (header {header!r})")
    records = [json.loads(line) for line in lines[1:] if line.strip()]

    origin = None
    usage = {key: None for key in USAGE_FIELDS}
    summary_usage = {key: None for key in USAGE_FIELDS}
    responses_without_usage = 0
    replacements_with_usage = 0
    # Was any usage journalled at all, and did one of those usages leave an input
    # sub-field null (so `input_total` is a lower bound)?
    usage_seen = False
    input_incomplete = False
    counts = {"requests": 0, "interrupted_responses": 0, "user_inputs": 0, "inbox_messages": 0,
              "context_replacements": 0, "environment_records": 0, "provider_retries": 0}
    tool_calls = {}                     # status -> count
    by_tool = {}                        # tool name -> count
    shell_calls = set()
    delegate_calls = set()
    workers_started = []                # ids of `worker_start` results, in order
    shell_exits = {"zero": 0, "non_zero": 0, "no_exit_code": 0}
    started = set()
    finished = set()
    # §3c: the run of context replacements since the last progress, and whether the
    # journal ends on an interrupted (never-completed) response.
    idle_run = 0
    max_idle_run = 0
    last_response = None

    for record in records:
        kind = record["record"]
        if kind in ("assistant_completed", "assistant_interrupted"):
            last_response = kind
        if kind == "environment":
            counts["environment_records"] += 1
            origin = record["route"]["origin"]
        elif kind == "user_input":
            counts["user_inputs"] += 1
            if record["text"] == PROVIDER_RETRY_MESSAGE:
                counts["provider_retries"] += 1
        elif kind == "inbox":
            counts["inbox_messages"] += 1
        elif kind == "assistant_completed":
            counts["requests"] += 1
            if record.get("usage") is None:
                responses_without_usage += 1
            else:
                usage_seen = True
                if any(record["usage"].get(key) is None for key in INPUT_FIELDS):
                    input_incomplete = True
                for key in usage:
                    usage[key] = add(usage[key], record["usage"].get(key))
        elif kind == "assistant_interrupted":
            counts["requests"] += 1
            counts["interrupted_responses"] += 1
        elif kind == "context_replaced":
            counts["context_replacements"] += 1
            if record.get("usage") is not None:
                replacements_with_usage += 1
                usage_seen = True
                if any(record["usage"].get(key) is None for key in INPUT_FIELDS):
                    input_incomplete = True
                for key in summary_usage:
                    summary_usage[key] = add(summary_usage[key], record["usage"].get(key))
            idle_run += 1
            max_idle_run = max(max_idle_run, idle_run)
        elif kind == "tool_started":
            started.add(record["call_id"])
            if record["identity"]["implementation"] == SHELL_IMPLEMENTATION:
                shell_calls.add(record["call_id"])
            if record["identity"]["implementation"] == DELEGATION_IMPLEMENTATION:
                delegate_calls.add(record["call_id"])
        elif kind == "tool_finished":
            result = record["result"]
            finished.add(result["call_id"])
            tool_calls[result["status"]] = tool_calls.get(result["status"], 0) + 1
            by_tool[result["name"]] = by_tool.get(result["name"], 0) + 1
            if (result["call_id"] in delegate_calls and result["status"] == "ok"
                    and result["content"].startswith(WORKER_STARTED_PREFIX)):
                worker_id = result["content"][len(WORKER_STARTED_PREFIX):].split(" ", 1)[0]
                if worker_id:
                    workers_started.append(worker_id)
            if result["name"] == FINISH_TOOL or (
                    result["name"] in MUTATING_TOOLS and result["status"] == "ok"):
                idle_run = 0
            if result["call_id"] in shell_calls and result["status"] == "ok":
                match = EXIT_CODE.search(result["content"])
                if match is None:
                    shell_exits["no_exit_code"] += 1
                elif int(match.group(1)) == 0:
                    shell_exits["zero"] += 1
                else:
                    shell_exits["non_zero"] += 1

    input_total = input_total_of(usage)
    usage_total = usage_sum(usage, summary_usage)
    total_calls = sum(tool_calls.values())
    return {
        "origin": origin,
        "records": len(records),
        **counts,
        "tool_calls": total_calls,
        "tool_calls_by_status": tool_calls,
        "tool_calls_not_ok": total_calls - tool_calls.get("ok", 0),
        "tool_calls_by_tool": by_tool,
        "tool_calls_started_without_result": len(started - finished),
        "shell_exits": shell_exits,
        "usage": usage,
        "input_total": input_total,
        "summary_usage": summary_usage,
        "replacements_with_usage": replacements_with_usage,
        "usage_total": usage_total,
        "input_total_all": input_total_of(usage_total),
        "input_total_complete": None if not usage_seen else not input_incomplete,
        "workers_started": workers_started,
        "cache_read_share": (round(usage["cache_read"] / input_total, 3)
                             if input_total and usage["cache_read"] is not None else None),
        "responses_without_usage": responses_without_usage,
        "stalled_on_summaries": (max_idle_run >= max_idle_summaries
                                 and last_response == "assistant_interrupted"),
    }


def worker_files(path):
    """The `FILE.w<N>.jsonl` worker journals next to a session, ordered by number."""
    found = []
    for candidate in glob.glob(glob.escape(path) + ".w*.jsonl"):
        match = WORKER_FILE.search(candidate)
        if match is not None:
            found.append((int(match.group(1)), candidate))
    found.sort()
    return found


def report(path, max_idle_summaries=DEFAULT_MAX_IDLE_SUMMARIES):
    parent = analyze(path, max_idle_summaries)
    workers = []
    for number, worker_path in worker_files(path):
        stats = analyze(worker_path, max_idle_summaries)
        workers.append({
            "id": f"w{number}",
            "origin": stats["origin"],
            "requests": stats["requests"],
            "tool_calls": stats["tool_calls"],
            "usage": stats["usage"],
            "input_total": stats["input_total"],
            "summary_usage": stats["summary_usage"],
            "replacements_with_usage": stats["replacements_with_usage"],
        })
    # Parent plus workers; a part is null only when it is unknown everywhere.
    usage_with_workers = usage_sum(parent["usage"], *(worker["usage"] for worker in workers))
    summary_usage_with_workers = usage_sum(parent["summary_usage"],
                                           *(worker["summary_usage"] for worker in workers))
    usage_total_with_workers = usage_sum(usage_with_workers, summary_usage_with_workers)
    if workers:
        worker_usage_known = True
    elif parent["workers_started"]:
        worker_usage_known = False
    else:
        worker_usage_known = None
    return {
        "session": path,
        **parent,
        "workers": workers,
        "usage_with_workers": usage_with_workers,
        "summary_usage_with_workers": summary_usage_with_workers,
        "usage_total_with_workers": usage_total_with_workers,
        "input_total_with_workers": input_total_of(usage_with_workers),
        "input_total_all_with_workers": input_total_of(usage_total_with_workers),
        "includes_worker_usage": bool(workers),
        "worker_usage_known": worker_usage_known,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("session")
    parser.add_argument("--label")
    parser.add_argument("--elapsed", type=float, help="wall-clock seconds of the run")
    parser.add_argument("--exit-code", type=int)
    parser.add_argument("--accepted", choices=["yes", "no", "unknown"], default="unknown",
                        help="result accepted after INDEPENDENT verification")
    parser.add_argument("--interventions", type=int, help="times the operator had to step in")
    parser.add_argument("--note")
    parser.add_argument("--harness-head", help="the p1 repository's revision when the run started")
    parser.add_argument("--binary-sha256", help="hash of the p1 binary that ran (identifies the build)")
    parser.add_argument("--max-idle-summaries", type=int, default=DEFAULT_MAX_IDLE_SUMMARIES,
                        help="the run's --max-idle-summaries (default: 6); only used for "
                             "stalled_on_summaries")
    parser.add_argument("--append", metavar="FILE", help="also append the record to FILE")
    args = parser.parse_args()

    record = report(args.session, args.max_idle_summaries)
    record.update({"label": args.label, "elapsed_seconds": args.elapsed,
                   "exit_code": args.exit_code, "accepted": args.accepted,
                   "operator_interventions": args.interventions, "note": args.note,
                   "harness_head": args.harness_head, "binary_sha256": args.binary_sha256})
    line = json.dumps(record, ensure_ascii=False, sort_keys=True)
    print(line)
    if args.append:
        with open(args.append, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")


if __name__ == "__main__":
    main()
