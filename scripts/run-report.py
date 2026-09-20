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

With `--session FILE`, each worker `w<N>` writes its own `FILE.w<N>.jsonl`; those files are
discovered automatically and reported under `workers`, with `usage_with_workers` and
`input_total_with_workers` adding them to the parent's numbers. `includes_worker_usage` is
true exactly when at least one worker file was read (no worker file means no workers — a
child session without `--session` stays in memory and cannot be read back).
"""
import argparse
import glob
import json
import re
import sys

EXIT_CODE = re.compile(r"\[exit code: (-?\d+)\]\s*$")
WORKER_FILE = re.compile(r"\.w(\d+)\.jsonl$")
SHELL_IMPLEMENTATION = "p1-tool-shell"


def add(total, value):
    """Sum of known values; None while nothing is known."""
    if value is None:
        return total
    return value if total is None else total + value


def analyze(path):
    """Every metric of ONE journal file (the parent's, or one worker's)."""
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    header = json.loads(lines[0])
    if header.get("p1_journal") != 1:
        sys.exit(f"{path}: not a p1 journal (header {header!r})")
    records = [json.loads(line) for line in lines[1:] if line.strip()]

    origin = None
    usage = {key: None for key in
             ("input_uncached", "cache_read", "cache_write", "output", "reasoning_output",
              "cost_micro_usd")}
    responses_without_usage = 0
    counts = {"requests": 0, "interrupted_responses": 0, "user_inputs": 0, "inbox_messages": 0,
              "context_replacements": 0, "environment_records": 0}
    tool_calls = {}                     # status -> count
    by_tool = {}                        # tool name -> count
    shell_calls = set()
    shell_exits = {"zero": 0, "non_zero": 0, "no_exit_code": 0}
    started = set()
    finished = set()

    for record in records:
        kind = record["record"]
        if kind == "environment":
            counts["environment_records"] += 1
            origin = record["route"]["origin"]
        elif kind == "user_input":
            counts["user_inputs"] += 1
        elif kind == "inbox":
            counts["inbox_messages"] += 1
        elif kind == "assistant_completed":
            counts["requests"] += 1
            if record.get("usage") is None:
                responses_without_usage += 1
            else:
                for key in usage:
                    usage[key] = add(usage[key], record["usage"].get(key))
        elif kind == "assistant_interrupted":
            counts["requests"] += 1
            counts["interrupted_responses"] += 1
        elif kind == "context_replaced":
            counts["context_replacements"] += 1
        elif kind == "tool_started":
            started.add(record["call_id"])
            if record["identity"]["implementation"] == SHELL_IMPLEMENTATION:
                shell_calls.add(record["call_id"])
        elif kind == "tool_finished":
            result = record["result"]
            finished.add(result["call_id"])
            tool_calls[result["status"]] = tool_calls.get(result["status"], 0) + 1
            by_tool[result["name"]] = by_tool.get(result["name"], 0) + 1
            if result["call_id"] in shell_calls and result["status"] == "ok":
                match = EXIT_CODE.search(result["content"])
                if match is None:
                    shell_exits["no_exit_code"] += 1
                elif int(match.group(1)) == 0:
                    shell_exits["zero"] += 1
                else:
                    shell_exits["non_zero"] += 1

    input_total = None
    if usage["input_uncached"] is not None:
        input_total = usage["input_uncached"] + (usage["cache_read"] or 0) + (usage["cache_write"] or 0)
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
        "cache_read_share": (round(usage["cache_read"] / input_total, 3)
                             if input_total and usage["cache_read"] is not None else None),
        "responses_without_usage": responses_without_usage,
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


def report(path):
    parent = analyze(path)
    workers = []
    for number, worker_path in worker_files(path):
        stats = analyze(worker_path)
        workers.append({
            "id": f"w{number}",
            "origin": stats["origin"],
            "requests": stats["requests"],
            "tool_calls": stats["tool_calls"],
            "usage": stats["usage"],
            "input_total": stats["input_total"],
        })
    # Parent plus workers; a part is null only when it is unknown everywhere.
    usage_with_workers = dict(parent["usage"])
    for worker in workers:
        for key in usage_with_workers:
            usage_with_workers[key] = add(usage_with_workers[key], worker["usage"][key])
    input_total_with_workers = None
    if usage_with_workers["input_uncached"] is not None:
        input_total_with_workers = (usage_with_workers["input_uncached"]
                                    + (usage_with_workers["cache_read"] or 0)
                                    + (usage_with_workers["cache_write"] or 0))
    return {
        "session": path,
        **parent,
        "workers": workers,
        "usage_with_workers": usage_with_workers,
        "input_total_with_workers": input_total_with_workers,
        "includes_worker_usage": bool(workers),
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
    parser.add_argument("--append", metavar="FILE", help="also append the record to FILE")
    args = parser.parse_args()

    record = report(args.session)
    record.update({"label": args.label, "elapsed_seconds": args.elapsed,
                   "exit_code": args.exit_code, "accepted": args.accepted,
                   "operator_interventions": args.interventions, "note": args.note})
    line = json.dumps(record, ensure_ascii=False, sort_keys=True)
    print(line)
    if args.append:
        with open(args.append, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")


if __name__ == "__main__":
    main()
