---
adr: 22
title: Interrupted calls are reconciled, never re-executed
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/core.md, docs/design/journal.md, c12be66, cf61082]
---
# ADR-0022: Interrupted calls are reconciled, never re-executed

## Context

A crash or a failed commit can leave a tool call without a recorded result. `seams.md`
section 7: a crash after a side effect but before its result is ambiguous, and "a JSONL log
does not grant exactly-once execution". `core.md` ruling R5 adds the same case for a commit
failure at `ToolStarted`/`ToolFinished`.

## Decision

One reconciliation rule serves both resume and commit failure. On the next turn (or on
resume), every unresolved call of the last assistant item is resolved in block order: a call
whose `ToolStarted` was committed gets `ToolFinished{status: Unknown}` with the fixed
`Interrupted: this call was started before the session stopped and its outcome is unknown. Check the current state before retrying.` message; a call without `ToolStarted` gets
`Cancelled before execution`. Nothing is re-executed and authorization is not asked. Ruling R2
says the failed record's `seq` is reused, and R6 says an event is emitted only after its
record commits.

## Consequences

The resumed history is well-formed (every call has a result) and the model decides what
to do with the truth in front of it. Side effects are never silently repeated. The cost is
that an interrupted call's outcome is genuinely unknown until the model checks.

## Alternatives considered

Re-executing the call automatically (rejected: the side effect may already have
happened); dropping the unresolved call (would leave a malformed history). `journal.md` says
the call "is NEVER re-executed automatically" (`docs/design/journal.md`).

## Evidence

`docs/design/core.md` rulings R2, R5 and R6; `docs/design/journal.md`
("Interrupted-call reconciliation"). `docs/SLICE-REPORT.md` acceptance 5. Commit c12be66
("Core: resolve unpaired tool calls at turn start (R5)") and cf61082 (rulings R5-R6).
