---
adr: 162
title: Workflow steps continue after transient provider failures
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/183, docs/design/workflows.md, crates/p1-host/tests/workflow_fallback.rs]
---
# ADR-0162: Workflow steps continue after transient provider failures

## Context

Issue #183 reports a workflow step lost to a mid-answer transport timeout.
Headless parent turns already recover under ADR-0041, but workflow steps reported
the first provider failure directly to the fallback chain. The issue's other case,
a final answer without `finish`, already has one repair under ADR-0072.

## Decision

The host continues a workflow step in its existing worker after Transport,
RateLimited or Protocol failures that interrupt streamed answer output (text,
reasoning or tool input), using ADR-0041's message, delays and consecutive retry
budget. Pre-answer failures still immediately use ADR-0054's route fallback;
once stream recovery begins, subsequent transient failures share its budget.
`p1 workflow run` accepts `--provider-retries N` (default 3; zero
disables); workflows started by an agent inherit its budget. Completed responses
reset the consecutive count. Cancellation interrupts waits; permanent failures
and exhausted retries retain the existing route-fallback behavior.

## Consequences

Committed tools and their results stay in the worker history; partial responses
are not replayed and tools are not blindly re-executed. Retries are recovery within
one dispatch, not new workflow cap charges. Schema repairs and finish nudges use
the same recovery but still get only one repair. Retry delays are visible before
the wait and injectable in tests; rate-limit recovery can wait several minutes.

## Alternatives considered

Retry in the provider adapter: cannot safely replay a stream after output started.
Start a fresh worker: loses committed work and may repeat side effects. Fall back
immediately: wastes an otherwise healthy route on a transient failure.

## Evidence

`cargo test -p p1-host --test workflow_fallback` exercises mid-answer recovery,
retry exhaustion and disabling, completed-tool progress, cancellation during the
wait, pre-answer fallback and the existing finish nudge. The frozen
`workflow_claude_fallback` test exposed an overbroad initial implementation that
waited on pre-answer account quota instead of falling back; its fixture is unchanged.
The recovery test failed before the fix:
the result was the fallback's answer rather than the original worker's answer.
