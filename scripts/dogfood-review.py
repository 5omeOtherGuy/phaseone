#!/usr/bin/env python3
"""Record inspectable run outcomes without retaining prompt-bearing raw streams.

Inputs are private invocation scratch. Output is an aggregate-only JSON record: no
journal text, model messages, tool arguments, stderr lines or file names are copied.
"""
import argparse
import json
import re
from pathlib import Path


def summarize(report: dict, stdout: bytes, stderr: bytes, journal: bytes,
              diff_numstat: bytes) -> dict:
    tool_results = []
    for raw in journal.splitlines():
        record = json.loads(raw)
        if record.get("record") != "tool_finished":
            continue
        result = record.get("result", {})
        content = result.get("content", "")
        exit_match = re.search(r"\[exit code: (-?\d+)\]\s*$", content)
        tool_results.append({
            "index": len(tool_results),
            "ok": result.get("status") == "ok",
            "exit_code": int(exit_match.group(1)) if exit_match else None,
        })
    stderr_lines = stderr.splitlines()
    insertions = deletions = changed_files = 0
    # A rename or copy is three records under `-z`: an `<add>\t<del>\t` header, then the
    # old and new paths as separate NUL-terminated tokens. The path tokens must be
    # consumed with their header, or they are read as numstat entries of their own and
    # the two-field index below raises IndexError (Codex finding dogfood-review.py:31).
    tokens = diff_numstat.split(b"\0")
    index = 0
    while index < len(tokens):
        token = tokens[index]
        index += 1
        if not token:
            continue
        fields = token.split(b"\t", 2)
        if len(fields) < 2:
            continue
        if len(fields) == 3 and fields[2] == b"":
            index += 2
        changed_files += 1
        if fields[0].isdigit():
            insertions += int(fields[0])
        if fields[1].isdigit():
            deletions += int(fields[1])
    return {
        "exit_code": report.get("exit_code"),
        "tool_calls": report.get("tool_calls"),
        "tool_calls_by_status": report.get("tool_calls_by_status"),
        "tool_calls_started_without_result": report.get("tool_calls_started_without_result"),
        "shell_exits": report.get("shell_exits"),
        "stdout_bytes": len(stdout),
        "stderr_bytes": len(stderr),
        "stderr_lines": len(stderr_lines),
        "stderr_error_lines": sum(b'error' in line.lower() or b'panic' in line.lower()
                                  for line in stderr_lines),
        "tool_results": tool_results,
        "changed_files": changed_files,
        "insertions": insertions,
        "deletions": deletions,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report")
    parser.add_argument("stdout")
    parser.add_argument("stderr")
    parser.add_argument("journal")
    parser.add_argument("diff_numstat")
    parser.add_argument("out")
    args = parser.parse_args()
    report = json.loads(Path(args.report).read_text(encoding="utf-8"))
    record = summarize(report, Path(args.stdout).read_bytes(), Path(args.stderr).read_bytes(),
                       Path(args.journal).read_bytes(), Path(args.diff_numstat).read_bytes())
    Path(args.out).write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
