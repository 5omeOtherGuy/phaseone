#!/usr/bin/env python3
"""Read the per-request timing out of a p1 session journal (ADR-0121, issue #422).

A version-3 journal carries a ``request_timing`` record right after each request's
``assistant_completed`` (or ``assistant_interrupted``). This script reads one and
prints, per request, four derived figures:

    time to first event   first_event_ms - sent_ms   (bytes/headers arrived)
    time to first output  first_output_ms - sent_ms  (first token of any kind)
    decode                ended_ms - first_output_ms (generating after the first token)
    waits                 how many, and how long in total (sum of delay_ms)

Usage:
    journal-timing.py SESSION.jsonl

Everything is millisecond wall-clock time read from the record; nothing here
measures the clock itself. A missing sub-field prints ``n/a``. A journal without
times — a version-1 or version-2 file, which carries no ``at_ms`` and no
``request_timing`` — prints ``unknown`` instead of figures (ADR-0121 point 5).
"""

from __future__ import annotations

import json
import sys


def journal_version(path):
    """The ``p1_journal`` header value, or ``None`` when the file carries no header."""
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                return None
            if isinstance(value, dict):
                version = value.get("p1_journal")
                return version if isinstance(version, int) else None
            return None
    return None


def analyze(path):
    """Return one dict per ``request_timing`` record, in file order, each with the
    four derived figures (``None`` when a sub-field they need is absent)."""
    requests = []
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict) and value.get("record") == "request_timing":
                requests.append(_derive(value))
    return requests


def _derive(timing):
    sent = timing.get("sent_ms")
    first_event = timing.get("first_event_ms")
    first_output = timing.get("first_output_ms")
    ended = timing.get("ended_ms")
    waits = timing.get("waits") or []
    return {
        "request_index": timing.get("request_index"),
        "time_to_first_event_ms": _delta(first_event, sent),
        "time_to_first_output_ms": _delta(first_output, sent),
        "decode_ms": _delta(ended, first_output),
        "wait_count": len(waits),
        "wait_total_ms": sum(wait.get("delay_ms", 0) for wait in waits),
    }


def _delta(end, start):
    if end is None or start is None:
        return None
    return end - start


def _ms(value):
    return "n/a" if value is None else f"{value} ms"


def format_request(derived):
    return (
        f"request {derived['request_index']}: "
        f"time to first event {_ms(derived['time_to_first_event_ms'])}, "
        f"time to first output {_ms(derived['time_to_first_output_ms'])}, "
        f"decode {_ms(derived['decode_ms'])}, "
        f"waits {derived['wait_count']} ({derived['wait_total_ms']} ms)"
    )


def main(argv):
    if len(argv) != 2:
        print("usage: journal-timing.py SESSION.jsonl", file=sys.stderr)
        return 2
    version = journal_version(argv[1])
    requests = analyze(argv[1])
    if requests:
        for derived in requests:
            print(format_request(derived))
    elif version is not None and version < 3:
        print(f"timing: unknown (a version-{version} journal carries no times)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
