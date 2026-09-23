---
adr: 54
title: Workflow roles have a fallback chain for route failures; DeepSeek is the shipped worker
status: proposed
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, crates/p1-workflow/src/api.rs, docs/design/workflows.md]
---
# ADR-0054: Workflow roles have a fallback chain for route failures; DeepSeek is the shipped worker

## Context

ADR-0053 item 4 ships the `worker` and `reviewer` roles on Claude Opus 5.5 and says "no
substitute model" when a cap refuses an attempt. The owner, 2026-09-23, reviewing the landed
module: "Deepseek v4.1 flash should be the primary worker with a fallback chain." DeepSeek V4.1
Flash is the cheap, proven worker (20+ accepted briefs, the whole modularity audit), but its only
route today is one subscription that runs out (the primary OpenCode Go account is already out
of credit; `deepseek2` was at ~85 % during the night). A workflow of fifty steps must not die
because the route it started on is exhausted halfway; it also must not quietly switch to a
model the owner capped.

## Decision

1. **Shipped defaults change.** `worker` = `deepseek2/deepseek-v4.1-flash` with grant
   `[read, grep, edit, shell]`; `reviewer` = `claude/claude-opus-5-5:high`; `verifier` =
   `deepseek2/deepseek-v4.1-flash`; `judge` = `claude/claude-fable-5` (cap 3) — unchanged
   where not named.
2. **A role may name a fallback chain.** `[workflows.roles.<r>] model = "E/P[:effort]"` gains
   `fallback = ["E/P[:effort]", …]` (ordered, may be empty). Shipped: `worker.fallback =
   ["gpt/gpt-6-sol", "claude/claude-opus-5-5"]`, `verifier.fallback = ["gpt/gpt-6-sol"]`; the
   reviewer and judge have none.
3. **Fallback is for route failures only.** A step moves to the next model in the chain when
   its worker could not run at all or ended on a provider failure of the route: an exhausted
   account (ADR-0046's error kind), the route unreachable or refusing the model, a turn that
   ends on a provider error after the host's own retries. It never applies to a cap
   (`quota_exceeded` stays final, ADR-0053 item 4), to a `blocked` or `failed` step that
   ran (a wrong answer is not a route failure), to a schema failure (repair stays on the same
   worker) or to a cancelled run.
4. **Each attempt counts and is visible.** Every model tried is a journalled dispatch that
   charges that model's cap; the step line and the envelope name the chain walked
   (`worker → deepseek2 exhausted → gpt/gpt-6-sol; w7`), and the run report counts
   `fell_back`. A step whose whole chain failed ends `failed — route: <last error>`.
5. **Resume honours the chain as it was walked**: replayed steps keep their recorded model;
   re-run steps start the chain from its head again.

## Consequences

- The everyday worker is the cheap one, and an exhausted route degrades a run to the next
  candidate instead of killing it; the cost is visible per model in the report.
- The owner's cap on Fable stays absolute: no chain may reach a capped model past its cap.
- The model-cards "replacement trial" (route bounded briefs away from DeepSeek) is a policy
  for orchestrating agents, not for workflow defaults; the owner's decision here is explicit.

## Alternatives considered

- Fallback on any failure: hides wrong answers behind a model change; rejected.
- Fallback on `quota_exceeded`: would defeat the cap; rejected.
- Keep Opus as the shipped worker: the owner's decision is DeepSeek.

## Evidence

<filled when merged: `cargo test -p p1-workflow` and `-p p1-host --test workflow_*` including
the new fallback tests; one live run on `deepseek2` with the route forced to fail.>
