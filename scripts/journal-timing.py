#!/usr/bin/env python3
"""Read the per-request timing out of a p1 session journal (ADR-0121, issue #422).

A version-3 journal carries a ``request_timing`` record right after each request's
``assistant_completed`` (or ``assistant_interrupted``). This script reads one and
prints, per request, these derived figures:

    time to first event   first_event_ms - sent_ms   (bytes/headers arrived)
    time to first output  first_output_ms - sent_ms  (first token of any kind)
    decode                ended_ms - first_output_ms (generating after the first token)
    waits                 how many, and how long in total (sum of delay_ms)
    gap before            sent_ms - at_ms of the last record committed before the request
    tools                 how many calls the request asked for, and their span: last
                          tool_finished at_ms - first tool_started at_ms
    idle after            the next request's sent_ms - at_ms of the last record
                          committed before it (n/a for the last request)

The request figures come from the core's clock and ``at_ms`` from the store's; both
are the system clock in a real run, so the gaps are comparable there.

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


def _records(path):
    """Every record line of the journal, in file order (the header is skipped)."""
    records = []
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict) and "record" in value:
                records.append(value)
    return records


_ASSISTANT = ("assistant_completed", "assistant_interrupted")


def analyze(path):
    """Return one dict per ``request_timing`` record, in file order, with the derived
    figures (``None`` when a value they need is absent)."""
    records = _records(path)
    timings = [i for i, r in enumerate(records) if r.get("record") == "request_timing"]
    requests = []
    for n, i in enumerate(timings):
        derived = _derive(records[i])
        derived["gap_before_ms"] = _gap_before(records, i, records[i].get("sent_ms"))
        started, finished = [], []
        for record in records[i + 1 :]:
            kind = record.get("record")
            if kind in _ASSISTANT:
                break
            if kind == "tool_started":
                started.append(record.get("at_ms"))
            elif kind == "tool_finished":
                finished.append(record.get("at_ms"))
        derived["tool_count"] = len(started)
        derived["tool_span_ms"] = (
            _delta(max(finished), min(started))
            if started and finished and None not in started + finished
            else None
        )
        if n + 1 < len(timings):
            following = timings[n + 1]
            derived["idle_after_ms"] = _gap_before(
                records, following, records[following].get("sent_ms")
            )
        else:
            derived["idle_after_ms"] = None
        requests.append(derived)
    return requests


def _gap_before(records, timing_index, sent):
    """``sent`` minus the ``at_ms`` of the last record committed before the request's
    assistant record (the record just before the ``request_timing`` line)."""
    j = timing_index - 1
    while j >= 0 and records[j].get("record") not in _ASSISTANT:
        j -= 1
    if j <= 0:
        return None
    return _delta(sent, records[j - 1].get("at_ms"))


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
        f"waits {derived['wait_count']} ({derived['wait_total_ms']} ms), "
        f"gap before {_ms(derived['gap_before_ms'])}, "
        f"tools {derived['tool_count']} (span {_ms(derived['tool_span_ms'])}), "
        f"idle after {_ms(derived['idle_after_ms'])}"
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
