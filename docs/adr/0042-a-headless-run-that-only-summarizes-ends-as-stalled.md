---
adr: 42
title: A headless run that only summarizes ends as stalled
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/completion.md, docs/dogfood/runs.jsonl, docs/adr/0041-a-headless-run-waits-and-continues-after-a-transient-provider-failure.md]
---
# ADR-0042: A headless run that only summarizes ends as stalled

## Context

Dogfood run split4a (2026-09-20) made 855 requests in 96 minutes: 698 file reads, 40 context
summaries, no edit, no change to the workspace. The `[context]` values were far too tight for
the task, so every summary discarded what the agent had just read and it read it again. Nothing
in the harness noticed. The run then ended on a `Protocol` failure, which ADR-0041 did not retry.

## Decision

In headless runs the host counts consecutive context replacements since the last progress (a
workspace mutation or a `finish` call). At `--max-idle-summaries` (default 6, 0 disables) it
cancels the turn and the run ends stalled, exit 4, with a message naming the likely causes.
`Protocol` failures join ADR-0041's transient kinds with the Transport schedule.

## Consequences

- A mis-sized context or an oversized job costs minutes, not hours and tens of millions of
  tokens, and the evidence record says why (`stalled_on_summaries`).
- A legitimately read-only unattended task with a tight context needs the flag.
- Only the parent agent is guarded; delegated workers are not yet.

## Alternatives considered

- Guard inside the context policy: it does not know about workspace progress; the host does (§3).
- A request or token ceiling per run: blunt, and it punishes long productive runs (split3b: 475
  requests, 14 summaries, accepted).

## Evidence

`docs/dogfood/runs.jsonl`: `split4a-deepseek` (accepted no) versus `split4a1-deepseek` (45
requests, 7.5 min, same harness after the context values were corrected).
`crates/p1-host/tests/stall_guard.rs`.
