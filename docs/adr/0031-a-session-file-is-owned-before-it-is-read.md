---
adr: 31
title: A session file is owned before it is read
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/review-2026-09-20-dispositions.md, docs/design/journal.md, crates/p1-journal/src/lib.rs, crates/p1-host/src/session.rs]
---
# ADR-0031: A session file is owned before it is read

## Context

`journal.md` promised that a second writer on one session file fails fast. The independent
review of 2026-09-20 (finding R6) showed the promise did not hold for resume: the host
loaded the file, repaired a truncated tail and only THEN opened the locked append handle,
and `repair_truncated_tail` took no lock at all. A second `p1 --resume` on a live session
could see the first writer's half-written line as a "truncated tail" and cut the file — a
committed-state loss — before finally being told the file was locked. `open_for_append`
also trusted a sequence number the caller had derived from an earlier, unlocked read.
This is a broken guarantee, so it is fixed; this ADR records the interface that fixes it.

## Decision

Resume is ONE operation that takes ownership first: `JsonlJournal::resume(path, sync)` takes
the exclusive advisory lock, then reads, validates, cuts off a truncated tail and derives the
next sequence number under that lock, and keeps the lock for the life of the returned writer.
The host resumes only through it. The older pieces stay for tools and tests but are safe on
their own: `repair_truncated_tail` takes the lock and refuses a tail that is no longer the
file's tail (`JournalError::StaleTail`); `open_for_append` locks before reading and refuses a
sequence number that does not match the file (`OutOfOrder`). `load` stays a lock-free read.

## Consequences

- A second process on a live session fails with `Locked` and has not touched the file.
- No decision is ever acted on from an observation made before ownership.
- `load` + `repair_truncated_tail` + `open_for_append` is no longer the resume recipe; new
  callers use `resume`.
- The lock is advisory (`flock`): it binds p1 processes, not arbitrary programs.

## Alternatives considered

- Lock inside `repair_truncated_tail` only: closes the reproduced hole but leaves the
  load → repair → append sequence acting on stale reads between its steps.
- A separate `.lock` file: another file to leak and clean up; locking the session file itself
  already works and is what writers did.

## Evidence

`cargo test -p p1-journal --test review` (the reviewer's reproduction, red before commit
`7cb79a8`), `--test lead_ownership` (locked resume leaves the file byte-identical; stale tail
and stale sequence are refused; the lock lives as long as the writer), and
`--test lead_crash_resume` (every-byte cut, unchanged, still green).
