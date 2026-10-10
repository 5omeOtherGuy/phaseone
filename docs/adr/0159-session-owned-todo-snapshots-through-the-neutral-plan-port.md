---
adr: 159
title: Session-owned todo snapshots through the neutral plan port
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0159: Session-owned todo snapshots through the neutral plan port

## Context

Issue #723 requires an agent-authored list, durable per session, with no core or
UI types. Workflow observations alone cannot show a single agent's future work.

## Decision

`todo_write` replaces the entire authored list (content, status, priority), rather
than merging by content: edits, removals and reordering must not leave stale tasks.
An empty list clears it. `p1-tool-todo/guest` owns validation and guidance;
`p1-module-todo` imports only cooperative cancellation (`control`). Successful tool content is the
canonical JSON snapshot, already redacted and journaled by the normal ToolFinished
boundary. The core and module protocol do not change.

The neutral `PlanSource` port in p1-contracts separates the source from consumers.
`p1-todo-session` projects committed records by implementation identity, not tool
face, and retains the latest valid successful snapshot. The host wraps its parent
journal and forwards only after commit through the default-no-op `plan_updated`
front-end hook. Resume replays the durable records and announces the latest list.
Workers have separate journals and cannot replace their parent's list.

ACP's existing plan mapper replaces its snapshot with the authored list. Workflow
steps remain the fallback until the first authored list; afterwards they cannot
overwrite it, even when explicitly cleared. Workflow cards continue independently.
Merging inferred execution steps into authored tasks would duplicate work and
invent task identities; the authored plan is the agent's authoritative list.

## Consequences

No new WIT import, dependency outside the workspace, or kernel state. Invalid,
failed, denied, cancelled and unfinished calls never change the list. Compaction
may remove old tool results from model history but not from the journal projection.
The result snapshot is part of the model's normal tool history.

## Alternatives considered

Core-owned plan records: refused by the microkernel boundary. A separate sidecar
file or tool-side state: refused because a commit failure could publish unrecorded
state. Merging snapshots: refused because items have no stable identity.

## Evidence

Guest tests cover complete replacement, clearing, all enums and bounds. Source
tests cover renamed tools, unsuccessful results, replay and session isolation.
Host tests cover durability-before-publication; `docs/acp/fixtures/todo-plan.jsonl`
replays the real module through the host and ACP driver without a live provider.
