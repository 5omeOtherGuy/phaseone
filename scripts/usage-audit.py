#!/usr/bin/env python3
"""Reproducible cache and compaction accounting over p1 session journals.

    scripts/usage-audit.py <runs dir | journal files...> [--json]
                           [--since YYYY-MM-DD] [--label SUBSTR]

Why this exists (perf audit 2026-09-23, corrected ranking item 2): the audit's cache and
compaction numbers came from one-off scripts and two accounting errors followed — a cache
share that ignored `cache_write`, and a compaction mean pooled across routes with
incomparable denominators. This script is the one command every later usage claim cites.

Cohort. Each argument is either a runs directory — searched recursively for `session.jsonl`
and `session.jsonl.wN.jsonl`, one journal per agent, parent and workers alike — or a journal
file. Every other file is ignored; a file that is read but whose first line is not
`{"p1_journal":1}` is reported under `skipped` with the reason, and the rest of the run is
still reported. `--since` keeps journals whose run directory is named `…-YYYYMMDD-HHMMSS`
with a date on or after the given day (a journal carries no timestamps, so a file outside
such a directory falls back to its modification date); `--label` keeps journals whose path
contains the substring. A worker journal is its own agent: it is never counted into its
parent's totals.

Formula. Cache share = `cache_read / (input_uncached + cache_read + cache_write)`, computed
only over the requests where all three are known; a request with an unknown one of them is
excluded, and a share over nothing is `unknown`, never 0. The share of a journal is that
pooled over its requests; per route and per model the report gives both the mean of the
per-journal shares and the pooled share, labelled, because the two answer different
questions. `cache_write` is what a route that caches must report for its share to exist: a
route that leaves it null (today every openai-chat route) has no three-term share at all.

Requests. A request is one `assistant_completed` record (an agent request) or the
summarizer request of one `context_replaced` record (a summary request) — the two are kept
apart everywhere. Per request the report carries `input_uncached`, `cache_read`,
`cache_write`, `output`, `reasoning_output`, `usage_present` (false when the record's
`usage` was null), the route and model of the journal's `environment` record, and whether it
is an agent or a summary request. An unknown field stays null, never 0; a per-journal sum
over known values carries `partial: true` when any request was unknown, so the sum is read
as a lower bound.

Per `context_replaced` the report also carries the summary request's usage and the cache
share of the FIRST agent request after it (the reuse the replacement left behind).

Output. Markdown tables on stdout, or with `--json` one JSON document with the same numbers
plus a per-request trace and a cohort manifest (each journal's path, byte size and record
count). Only numbers, identifiers, route and model names and paths ever appear: no prompt,
tool or message text is read out of a journal.

Reading is line by line: a whole journal is never held in memory as one string (the largest
today is 17 MB, all of them 88 MB).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
from datetime import date, datetime

# The `Usage` fields this report carries per request (cost is not an accounting input here).
USAGE_FIELDS = ("input_uncached", "cache_read", "cache_write", "output", "reasoning_output")
# The fields the cache-share denominator adds.
CACHE_FIELDS = ("input_uncached", "cache_read", "cache_write")
# The header of a p1 journal (docs/design/journal.md).
JOURNAL_HEADER = {"p1_journal": 1}
# A journal file: the parent's `session.jsonl` or a worker's `session.jsonl.wN.jsonl`.
JOURNAL_NAME = re.compile(r"^session\.jsonl(\.w\d+\.jsonl)?$")
WORKER_FILE = re.compile(r"\.w(\d+)\.jsonl$")
# A run directory: `<label>-YYYYMMDD-HHMMSS`.
RUN_DATE = re.compile(r"(\d{4})(\d{2})(\d{2})-\d{6}")
# The route/model bucket of a journal with no `environment` record.
UNKNOWN = "unknown"


class NotAJournal(Exception):
    """A file that is not a p1 journal (reported as skipped, never fatal)."""


def agent_of(path):
    """`(agent id, parent journal path or None)` — a worker file is its own agent."""
    match = WORKER_FILE.search(path)
    if match is None:
        return "parent", None
    return f"w{int(match.group(1))}", path[:match.start()]


def number(value):
    """A journalled token count: a non-negative int, else unknown (`None`), never 0."""
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return None
    return value


def parse_line(line):
    """The record on one line, or `None` when the line is not JSON."""
    try:
        return json.loads(line)
    except ValueError:
        return None


def journal_date(path):
    """The day a journal belongs to. A journal carries no timestamps, so this is the
    `<label>-YYYYMMDD-HHMMSS` run directory in its path, else the file's mtime."""
    found = None
    for match in RUN_DATE.finditer(path):
        found = match
    if found is not None:
        try:
            return date(int(found.group(1)), int(found.group(2)), int(found.group(3)))
        except ValueError:
            pass
    return date.fromtimestamp(os.path.getmtime(path))


def request_of(record, kind, route, model):
    """One request: its usage fields (unknown stays `None`) and who asked."""
    usage = record.get("usage")
    present = isinstance(usage, dict)
    request = {"kind": kind, "route": route, "model": model, "usage_present": present}
    for field in USAGE_FIELDS:
        request[field] = number(usage.get(field)) if present else None
    return request


def pooled_cache_share(requests):
    """`cache_read / (input_uncached + cache_read + cache_write)` over the requests where
    all three are known and the denominator is non-zero. `(None, 0)` when nothing
    qualifies: a share over no requests is unknown, never 0."""
    numerator = denominator = contributing = 0
    for request in requests:
        values = [request[field] for field in CACHE_FIELDS]
        if any(value is None for value in values):
            continue
        total = sum(values)
        if total == 0:
            continue
        numerator += request["cache_read"]
        denominator += total
        contributing += 1
    if denominator == 0:
        return None, 0
    return round(numerator / denominator, 4), contributing


def summarize(requests):
    """Counts and per-field sums over known values of a set of requests."""
    tokens = {field: None for field in USAGE_FIELDS}
    unknown_fields = {field: 0 for field in USAGE_FIELDS}
    unknown_usage = 0
    for request in requests:
        if not request["usage_present"]:
            unknown_usage += 1
        for field in USAGE_FIELDS:
            value = request[field]
            if value is None:
                unknown_fields[field] += 1
            else:
                tokens[field] = value if tokens[field] is None else tokens[field] + value
    return {"requests": len(requests), "unknown_usage_requests": unknown_usage,
            "unknown_fields": unknown_fields, "tokens": tokens,
            "partial": any(unknown_fields.values())}


def analyze_journal(path):
    """Every request-level number of ONE journal (one agent). Line by line; a file that is
    not a p1 journal raises `NotAJournal` with the reason."""
    agent, parent = agent_of(path)
    route = model = None
    requests = []
    compaction_detail = []
    waiting = []                     # compactions that still need their first agent request
    interrupted = malformed = records = 0
    with open(path, encoding="utf-8") as handle:
        first = handle.readline()
        if parse_line(first) != JOURNAL_HEADER:
            raise NotAJournal('first line is not {"p1_journal":1}')
        for line in handle:
            if not line.strip():
                continue
            record = parse_line(line)
            if not isinstance(record, dict):
                malformed += 1
                continue
            records += 1
            kind = record.get("record")
            if kind == "environment":
                origin = (record.get("route") or {}).get("origin") or {}
                route = origin.get("route", route)
                model = origin.get("model", model)
            elif kind == "assistant_completed":
                request = request_of(record, "agent", route, model)
                requests.append(request)
                for detail in waiting:
                    detail["first_after"] = request
                    detail["first_after_share"] = pooled_cache_share([request])[0]
                waiting = []
            elif kind == "assistant_interrupted":
                interrupted += 1
            elif kind == "context_replaced":
                request = request_of(record, "summary", route, model)
                requests.append(request)
                detail = {"summary": request, "summary_share": pooled_cache_share([request])[0],
                          "first_after": None, "first_after_share": None}
                compaction_detail.append(detail)
                waiting.append(detail)

    agent_requests = [request for request in requests if request["kind"] == "agent"]
    summary_requests = [request for request in requests if request["kind"] == "summary"]
    agent_stats = summarize(agent_requests)
    summary_stats = summarize(summary_requests)
    cache_share, cache_share_requests = pooled_cache_share(agent_requests)
    summary_share, summary_share_requests = pooled_cache_share(summary_requests)
    return {
        "path": path,
        "agent": agent,
        "parent": parent,
        "bytes": os.path.getsize(path),
        "records": records,
        "malformed_lines": malformed,
        "route": route,
        "model": model,
        "agent_requests": agent_stats["requests"],
        "interrupted_responses": interrupted,
        "summary_requests": summary_stats["requests"],
        "compactions": len(compaction_detail),
        "unknown_usage_requests": agent_stats["unknown_usage_requests"],
        "unknown_fields": agent_stats["unknown_fields"],
        "summary_unknown_usage_requests": summary_stats["unknown_usage_requests"],
        "summary_unknown_fields": summary_stats["unknown_fields"],
        "partial": agent_stats["partial"] or summary_stats["partial"],
        "tokens": agent_stats["tokens"],
        "summary_tokens": summary_stats["tokens"],
        "cache_share": cache_share,
        "cache_share_requests": cache_share_requests,
        "summary_cache_share": summary_share,
        "summary_cache_share_requests": summary_share_requests,
        "requests": requests,
        "compaction_detail": compaction_detail,
    }


def mean_uncached_per_summary(journals):
    """`(mean, n)` of `input_uncached` over the summary requests that know it."""
    total = count = 0
    for journal in journals:
        for request in journal["requests"]:
            if request["kind"] == "summary" and request["input_uncached"] is not None:
                total += request["input_uncached"]
                count += 1
    if count == 0:
        return None, 0
    return round(total / count, 1), count


def aggregate(journals, key):
    """One row per `route` (or per `model`): counts, both cache-share readings, and the
    mean uncached input of a summary request. Summary requests stay separate."""
    groups = {}
    for journal in journals:
        name = journal[key] if journal[key] is not None else UNKNOWN
        groups.setdefault(name, []).append(journal)
    rows = []
    for name in sorted(groups):
        members = groups[name]
        agent = summarize([request for journal in members for request in journal["requests"]
                           if request["kind"] == "agent"])
        summary = summarize([request for journal in members for request in journal["requests"]
                             if request["kind"] == "summary"])
        per_journal = [journal["cache_share"] for journal in members
                       if journal["cache_share"] is not None]
        pooled, pooled_requests = pooled_cache_share(
            [request for journal in members for request in journal["requests"]
             if request["kind"] == "agent"])
        summary_pooled, summary_pooled_requests = pooled_cache_share(
            [request for journal in members for request in journal["requests"]
             if request["kind"] == "summary"])
        mean_uncached, mean_uncached_requests = mean_uncached_per_summary(members)
        rows.append({
            "key": name,
            "journals": len(members),
            "agent_requests": agent["requests"],
            "agent_requests_unknown_usage": agent["unknown_usage_requests"],
            "agent_unknown_fields": agent["unknown_fields"],
            "summary_requests": summary["requests"],
            "summary_requests_unknown_usage": summary["unknown_usage_requests"],
            "summary_unknown_fields": summary["unknown_fields"],
            "journals_with_known_cache_share": len(per_journal),
            "mean_cache_share_per_journal": (round(sum(per_journal) / len(per_journal), 4)
                                             if per_journal else None),
            "pooled_cache_share": pooled,
            "pooled_cache_share_requests": pooled_requests,
            "summary_pooled_cache_share": summary_pooled,
            "summary_pooled_cache_share_requests": summary_pooled_requests,
            "mean_uncached_input_per_summary_request": mean_uncached,
            "summary_requests_with_known_uncached": mean_uncached_requests,
        })
    return rows


def collect(paths, since, label):
    """`(journal paths, filtered out)` — a directory is searched for journal files; every
    candidate is deduplicated, then filtered by `--since` and `--label`."""
    candidates = []
    for path in paths:
        if os.path.isdir(path):
            for root, _dirs, files in os.walk(path):
                for name in sorted(files):
                    if JOURNAL_NAME.match(name):
                        candidates.append(os.path.join(root, name))
        else:
            candidates.append(path)
    candidates.sort()
    journals = []
    seen = set()
    filtered = 0
    for path in candidates:
        marker = os.path.abspath(path)
        if marker in seen:
            continue
        seen.add(marker)
        if label is not None and label not in path:
            filtered += 1
            continue
        if since is not None:
            try:
                before = journal_date(path) < since
            except OSError:
                before = False      # unreadable: reported as skipped, not filtered away
            if before:
                filtered += 1
                continue
        journals.append(path)
    return journals, filtered


def build_document(journals, skipped, filtered, since, label):
    """The one JSON document: the same numbers the markdown shows, plus the per-request
    trace and the cohort manifest."""
    return {
        "script": "scripts/usage-audit.py",
        "formula": ("cache_read / (input_uncached + cache_read + cache_write), over the "
                    "requests where all three are known"),
        "filters": {"since": since, "label": label},
        "cohort": [{"path": journal["path"], "bytes": journal["bytes"],
                    "records": journal["records"], "agent": journal["agent"],
                    "parent": journal["parent"]} for journal in journals],
        "skipped": skipped,
        "journals": journals,
        "by_route": aggregate(journals, "route"),
        "by_model": aggregate(journals, "model"),
        "totals": {
            "journals": len(journals),
            "bytes": sum(journal["bytes"] for journal in journals),
            "records": sum(journal["records"] for journal in journals),
            "agent_requests": sum(journal["agent_requests"] for journal in journals),
            "summary_requests": sum(journal["summary_requests"] for journal in journals),
            "compactions": sum(journal["compactions"] for journal in journals),
            "interrupted_responses": sum(journal["interrupted_responses"] for journal in journals),
            "malformed_lines": sum(journal["malformed_lines"] for journal in journals),
            "agent_requests_unknown_usage": sum(journal["unknown_usage_requests"]
                                                for journal in journals),
            "skipped_files": len(skipped),
            "filtered_out": filtered,
        },
    }


def known(value):
    """A number or name as text, or `unknown` when the journal did not report it."""
    return UNKNOWN if value is None else str(value)


def share(value):
    """A cache share as a percentage, or `unknown` — never a number made of zeros."""
    return UNKNOWN if value is None else f"{value * 100:.1f} %"


def row(*cells):
    return "| " + " | ".join(str(cell) for cell in cells) + " |"


def table(add, header, rows):
    add(row(*header))
    add(row(*["---"] * len(header)))
    for cells in rows:
        add(row(*cells))


def render_markdown(doc):
    """The same numbers as the JSON, as tables."""
    lines = []
    add = lines.append
    totals = doc["totals"]
    filters = doc["filters"]
    add("# usage audit")
    add("")
    add(f"Formula: `{doc['formula']}`. A field a journal does not report stays `unknown`, "
        "never 0; a sum over known values is a lower bound when `partial` is set.")
    add("")
    add(f"- journals: {totals['journals']} ({totals['bytes']} bytes, "
        f"{totals['records']} records), skipped files: {totals['skipped_files']}, "
        f"filtered out: {totals['filtered_out']}")
    add(f"- agent requests: {totals['agent_requests']} "
        f"({totals['agent_requests_unknown_usage']} with unknown usage), "
        f"summary requests: {totals['summary_requests']}, "
        f"compactions: {totals['compactions']}, "
        f"interrupted responses: {totals['interrupted_responses']}, "
        f"malformed lines: {totals['malformed_lines']}")
    add(f"- filters: since={filters['since'] or 'none'}, label={filters['label'] or 'none'}")

    add("")
    add("## per journal")
    add("")
    table(add, ("journal", "agent", "route", "model", "agent req", "unknown",
                "summary req", "summary unknown", "cache share", "share req",
                "summary share", "partial", "compactions"),
          [(journal["path"], journal["agent"], known(journal["route"]), known(journal["model"]),
            journal["agent_requests"], journal["unknown_usage_requests"],
            journal["summary_requests"], journal["summary_unknown_usage_requests"],
            share(journal["cache_share"]), journal["cache_share_requests"],
            share(journal["summary_cache_share"]),
            "yes" if journal["partial"] else "no", journal["compactions"])
           for journal in doc["journals"]])

    add("")
    add("## tokens per journal (sums over known values)")
    add("")
    table(add, ("journal",) + USAGE_FIELDS + tuple(f"summary {field}" for field in USAGE_FIELDS),
          [(journal["path"],)
           + tuple(known(journal["tokens"][field]) for field in USAGE_FIELDS)
           + tuple(known(journal["summary_tokens"][field]) for field in USAGE_FIELDS)
           for journal in doc["journals"]])

    add("")
    add("## unknown usage fields per journal")
    add("")
    rows = []
    for journal in doc["journals"]:
        if journal["unknown_usage_requests"] or any(journal["unknown_fields"].values()):
            rows.append((journal["path"], journal["agent_requests"],
                         journal["unknown_usage_requests"])
                        + tuple(journal["unknown_fields"][field] for field in USAGE_FIELDS))
    if rows:
        table(add, ("journal", "agent req", "usage null") + USAGE_FIELDS, rows)
    else:
        add("every request of every journal carried every field.")

    add("")
    add("## compactions (per context_replaced)")
    add("")
    rows = []
    for journal in doc["journals"]:
        for detail in journal["compaction_detail"]:
            summary = detail["summary"]
            after = detail["first_after"]
            rows.append((journal["path"], known(journal["route"]), known(journal["model"]))
                        + tuple(known(summary[field]) for field in USAGE_FIELDS)
                        + (share(detail["summary_share"]),)
                        + tuple(known(after[field]) if after else UNKNOWN
                                for field in USAGE_FIELDS)
                        + (share(detail["first_after_share"]),))
    if rows:
        table(add, ("journal", "route", "model")
              + tuple(f"summary {field}" for field in USAGE_FIELDS)
              + ("summary share",)
              + tuple(f"after {field}" for field in USAGE_FIELDS)
              + ("after share",), rows)
    else:
        add("no compaction in the cohort.")

    for title, key in (("## per route", "by_route"), ("## per model", "by_model")):
        add("")
        add(title)
        add("")
        table(add, ("route" if key == "by_route" else "model", "journals", "agent req",
                    "agent unknown", "summary req", "summary unknown",
                    "mean share (per journal)", "journals with share", "pooled share",
                    "pooled req", "mean uncached input / summary req", "summary req with input"),
              [(group["key"], group["journals"], group["agent_requests"],
                group["agent_requests_unknown_usage"], group["summary_requests"],
                group["summary_requests_unknown_usage"],
                share(group["mean_cache_share_per_journal"]),
                group["journals_with_known_cache_share"],
                share(group["pooled_cache_share"]), group["pooled_cache_share_requests"],
                known(group["mean_uncached_input_per_summary_request"]),
                group["summary_requests_with_known_uncached"])
               for group in doc[key]])

    add("")
    add("## skipped")
    add("")
    if doc["skipped"]:
        table(add, ("path", "reason"),
              [(entry["path"], entry["reason"]) for entry in doc["skipped"]])
    else:
        add("none.")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("paths", nargs="+", metavar="PATH",
                        help="a runs directory (searched recursively for session.jsonl and "
                             "session.jsonl.wN.jsonl) or journal files")
    parser.add_argument("--json", action="store_true",
                        help="print one JSON document instead of markdown tables")
    parser.add_argument("--since", metavar="YYYY-MM-DD",
                        help="only journals of a run directory dated on or after this day")
    parser.add_argument("--label", metavar="SUBSTR",
                        help="only journals whose path contains SUBSTR")
    args = parser.parse_args(argv)

    since = None
    if args.since is not None:
        try:
            since = datetime.strptime(args.since, "%Y-%m-%d").date()
        except ValueError:
            parser.error("--since must be a date, YYYY-MM-DD")

    candidates, filtered = collect(args.paths, since, args.label)
    journals = []
    skipped = []
    for path in candidates:
        try:
            journals.append(analyze_journal(path))
        except NotAJournal as error:
            skipped.append({"path": path, "reason": str(error)})
        except (OSError, UnicodeDecodeError) as error:
            # The path is in `path`; the reason stays short and free of journal text.
            reason = getattr(error, "strerror", None) or error
            skipped.append({"path": path, "reason": f"cannot read: {reason}"})

    if not journals:
        print(f"usage-audit: no journals in {len(candidates)} file(s) "
              f"({len(skipped)} skipped, {filtered} filtered out)", file=sys.stderr)
        return 1

    doc = build_document(journals, skipped, filtered, args.since, args.label)
    if args.json:
        print(json.dumps(doc, indent=2, sort_keys=True, ensure_ascii=False))
    else:
        print(render_markdown(doc), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
