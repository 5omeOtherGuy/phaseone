---
adr: 34
title: Workers are not restored when their parent session resumes
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/review-2026-09-20-dispositions.md, docs/design/delegation.md, crates/p1-host/src/run.rs, crates/p1-tool-delegate/src/lib.rs, crates/p1-workers/src/lib.rs]
---
# ADR-0034: Workers are not restored when their parent session resumes

## Context

A worker is an in-process agent with a memory journal; its id, retained result and repair
state belong to the `InProcessWorkers` service of one process. The parent's JSONL session
survives that process; the workers do not. The independent review of 2026-09-20 (departure
D-D) noted that a resumed parent's history can therefore mention workers that no longer
exist, and that this needs an explicit, user-facing distinction before long delegated
sessions become normal. There was a second, sharper problem: a new service numbered its
workers from `w1` again, so after a resume `worker_result {"id":"w1"}` would have returned a
DIFFERENT worker's result under the old name.

## Decision

Resume is not restartable orchestration, and p1 says so instead of implying otherwise:
- Child sessions are not persisted and not restored.
- On resume the host finds the workers the journal started
  (`p1_tool_delegate::workers_started_in`), tells the user on stderr, and sends the model an
  inbox notification: those workers are gone, cannot be continued or queried, unfinished work
  was not saved, check the files, start a new worker if needed.
- Their ids are never reused: the new service reserves them (`reserve_ids`), so an old id
  answers `No worker <id>.` and a new worker gets the next number.

## Consequences

- The model sees the truth at its first request after the resume and decides what to redo —
  the same stance as for interrupted tool calls (ADR-0022): never re-execute silently.
- A worker's finished-but-unread result is lost with the process. Its file changes are not.
- Persistent child lifecycle (journalled children, re-attachable ids) stays possible later
  and would supersede this ADR; it is only worth building once longer delegated sessions work.

## Alternatives considered

- Persist and restore children now: a larger design (child journals, service state, ids
  across processes) that the review explicitly sequences after successful longer sessions.
- Say nothing and let `No worker w1.` speak for itself: leaves the model guessing why, and
  did not even hold — ids restarted at `w1`.

## Evidence

`cargo test -p p1-host --test lead_resume_decisions`
(`a_resumed_parent_is_told_its_earlier_workers_are_gone_and_ids_are_not_reused`).
