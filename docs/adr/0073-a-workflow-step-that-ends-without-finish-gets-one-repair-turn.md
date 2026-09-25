---
adr: 73
title: A workflow step that ends without finish gets one repair turn
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, docs/adr/0054-workflow-roles-have-a-fallback-chain-for-route-failures-deepseek-is-the-shipped-worker.md, crates/p1-workflow/src/engine.rs, docs/design/workflows.md]
---
# ADR-0073: A workflow step that ends without finish gets one repair turn

## Context

GitHub #183, second half: a workflow step whose worker ends its turn with a final answer
but no `finish` call fails `ended without finish` at once, although the answer is often
there and one more turn would have delivered it through `finish`. The live dogfood runs
show it (docs/dogfood/runs.jsonl: `live-check4-chain-gpt-low`, all three steps ended
without finish). ADR-0053 item 5 already gives a step whose result failed its schema ONE
repair turn in the same worker; ADR-0054 item 3 keeps that repair on the worker that
produced the result and reserves the fallback chain for route failures. The issue's first
half (transport failure) is covered by ADR-0054's chains and is not decided here.

## Decision

A step whose FIRST turn ends without `finish` is treated like a failed contract: the cap
is checked again (a capped nudge is the refused-repair envelope, ` (repair)`), then ONE
repair turn runs in the SAME worker through the existing repair plumbing with the message
`You ended your turn without calling finish. Call finish now: status "done" with your
result (and the evidence), or "blocked" with what you need.` That turn's end is the
step's end, `attempts: 2`; a second end without finish stays `ended without finish` with
the worker's last message as the value. At most one repair turn per step: a schema
repair turn that ends without finish gets no further nudge.

## Consequences

- A worker that forgot `finish` usually recovers for the price of one more dispatch,
  which is journalled (`attempt: 2`) and charged against the cap like a schema repair.
- A worker that never finishes now costs two dispatches instead of one before its step
  fails; `ended without finish` now means "after one repair turn".
- No second mechanism: the engine's one `repair` path carries both messages, and the
  host's `StepRunner::repair` (another turn of the same worker) is unchanged.
- The step never walks the fallback chain for this end (ADR-0054 item 3 unchanged).

## Alternatives considered

- Retrying on a new worker or the next link of the chain: rejected — the worker did run
  and has the context; ADR-0054 keeps the chain for route failures.
- More than one nudge: rejected — the same one-round bound as the schema repair keeps
  the cost of a step predictable.
- Leaving it to the script (`if r.error == "ended without finish" { agent(...) }`):
  rejected — a new worker loses the context and every script would repeat it.

## Evidence

`cargo test -p p1-workflow --test runs` —
`a_step_that_ends_without_finish_is_nudged_once_in_the_same_worker`,
`a_worker_that_never_finishes_gets_exactly_one_nudge`,
`a_capped_nudge_is_the_refused_repair_envelope`,
`a_schema_repair_that_ends_without_finish_is_not_nudged_again`; through the host,
`crates/p1-host/tests/workflow_fallback.rs` `a_step_that_ran_and_failed_does_not_fall_back`
(the nudge is the same worker's next message on the same route).
