---
adr: 21
title: The journal is the single truth; in-memory state is its projection
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/journal.md, docs/design/seams.md, docs/design/design-summary.md]
---
# ADR-0021: The journal is the single truth; in-memory state is its projection

## Context

`docs/design/design-summary.md` item 7 and `seams.md` section 7: one small append-only
record of what the model was actually sent and returned, with in-memory state as its
projection, for resumable long runs.

## Decision

One canonical append-only session journal with memory and JSONL backends. Records carry a
dense `seq`; both stores reject a gap or repeat. A JSONL file starts with a `{"p1_journal":1}`
header; unknown versions are an error. `EveryRecord` sync means `commit` returns after write
+ fsync (default), `OsBuffered` after write only; a crash's truncated final line is returned
as `TruncatedTail`, never silently dropped or "repaired". Commits happen at meaningful
boundaries; the core owns ordering and only a narrow commit sink, so no filesystem logic is in
the core.

## Consequences

Resume and replay preserve what was actually sent. The core has a small commit interface.
There is no exactly-once guarantee, no multi-writer safety on one file, no compaction and no
encryption in this slice.

## Alternatives considered

A Pi-style mirror of state (the design summary chooses "Journal as the single truth over
Pi-style mirror"); event-sourcing all UI, metrics, stream deltas and transient state (rejected
in `seams.md` section 7).

## Evidence

`docs/design/journal.md`. `docs/SLICE-REPORT.md` acceptance 5:
`cargo test -p p1-journal` and `--test lead_crash_resume` cut a real session at EVERY byte
offset and resume with no record lost or invented and a dense sequence. Commit d833577.
