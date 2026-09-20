---
adr: 36
title: Context control is a summarizing policy module with a durable, validated replacement
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/context.md, docs/design/core.md, crates/p1-context/src/lib.rs, crates/p1-context/tests/acceptance_sol.rs, crates/p1-core/tests/context_contract.rs]
---
# ADR-0036: Context control is a summarizing policy module with a durable, validated replacement

## Context

The first slice shipped only a passthrough context policy: a long session would overflow (owner
failure F5), and decisions and constraints were held by nothing but the raw transcript (F6).
The reviewer's plan amendment 4 set the bar for the spec: configurable thresholds and output
headroom; tool-call/result pairing and required reasoning replay preserved; cancellation and
summarization failure defined; durable replacement and faithful resume; evidence that task
constraints survive REPEATED replacements; and a model's capacity kept apart from the
empirically useful point to summarize.

## Decision

- The `ContextPolicy` contract changes: `prepare` receives the history, the last response's
  usage and the turn's cancellation token, and returns a replacement plus what producing it
  cost. The core races it against cancellation, VALIDATES call/result pairing before
  committing, journals `ContextReplaced{items, usage}` and announces it after the commit.
- `p1-context::SummarizingContext` is an optional module depending on the contracts only. It
  is stateless (a marker identifies summary items; everything else is in the history and the
  usage), summarizes through the agent's own provider, keeps whole units verbatim in the tail
  (so pairing and replay data hold by construction), keeps the user's messages verbatim within
  a budget, rolls the previous summary into the next one, and asks for fixed sections with a
  carry-forward rule for constraints and decisions.
- Two thresholds: `summarize_at_tokens` (the useful point) and the wall
  (`window_tokens - output_headroom_tokens`). Below the wall a failed summarization lets the
  turn continue and tries again; at the wall it fails the turn with both numbers. When nothing
  is left to summarize no request is made.
- Configuration is per environment (`[context]`, optional `summarize.md`); absent means
  passthrough. The shipped environments get values only from dogfooding measurements.

## Consequences

- Sessions can outlive the window; every replacement and its token cost is in the journal, and
  a resumed session continues from the replaced history.
- Summaries are lossy by nature. What is protected mechanically: the user's own words (within
  the budget), the recent tail, pairing, replay data. What is protected by the prompt only:
  constraints and decisions folded into the summary — shown to work live once, not proven.
- Token estimates are rough (chars/3.5) wherever a route reports no usage.
- A summarization request is an extra model call on the same route and subscription.

## Alternatives considered

- Dropping old items without a summary: cheap, and forgets exactly what F6 is about.
- Provider-side compaction: not available on both routes; would put policy into adapters.
- Summarizing the native history (tool calls as tool calls): ties the request to route
  validity rules; rendering to text works on every route.
- State in the module (remembering what was summarized): breaks faithful resume.

## Evidence

`cargo test -p p1-context` (28 frozen black-box tests by an independent author, written
before the implementation, plus internals), `cargo test -p p1-core --test context_contract`,
`cargo test -p p1-host --test context_wiring`. Live canary run recorded at the end of
`docs/design/context.md`: a constraint that existed only inside the summary was obeyed after
8 rolling replacements across 4 restarts.
