---
adr: 49
title: Model selection and switching a session to another model
status: accepted
date: 2026-09-21
deciders: owner+lead
supersedes: [33]
superseded_by: []
sources: [docs/design/model-selection.md, docs/adr/0033-a-session-resumes-only-on-the-route-and-model-that-recorded-it.md, crates/p1-core/src/resume.rs]
---
# ADR-0049: Model selection and switching a session to another model

## Context

Owner, 2026-09-21: "we are missing basic functionality. Model selection / scoped models like in
pi". p1 picks a model only through `--env`, whose environment names one route and one profile.
There is no way to pick another profile of the same route, to list models, to scope a set to
cycle through, or to change the model of a running session. ADR-0033 forbids the last one: a
session resumes only on the origin that recorded it, because nothing established that a route
accepts another model's history. It named the way out: "a deliberate compatibility policy (an
injected decision plus history translation), which would supersede this ADR".

## Decision

A model is an `environment/profile` pair (model-selection.md §1), chosen with `--model`,
`--effort`, `settings.toml` and scoped with `enabled_models` / `--models`. A running agent
switches with `Agent::reconfigure` between turns; the new environment is re-committed at the next
turn. The compatibility policy is the provider's own `validate`, now run against the CURRENT
history: the provider carries every history item it can (freeform calls translated to
function-shaped `{"input": …}` where the route has no freeform shape; foreign reasoning dropped
as before) and rejects the rest with a reason. The same rule decides resume, so
`ResumeError::RouteChanged` is removed.

## Consequences

- A switch or a resume on another model can be refused, with a reason, before anything is
  sent or committed; it is never silently lossy beyond dropping foreign reasoning.
- Every adapter's `validate` must inspect history items, and conformance gains checks for
  foreign-origin histories.
- Accepted after the live acceptance of model-selection.md §3 passed on every shipped route
  family; a family that later refuses a history shape is handled in its `validate`.

## Alternatives considered

- Keep ADR-0033 and select only at start: leaves the owner's request half-done.
- Summarise the history on every switch: loses detail and costs a request per switch.
- Translate every foreign call to text: lossy for the common same-family case, and not needed
  if routes accept foreign call names (to be measured live).

## Evidence

Live, 2026-09-21, lead, p1 at `9cd530f`: one session per switch, turn 1 on model A, turn 2
resumed with `--model B` (same code path as a live switch: `validate` over the projected
history, new `Environment` committed first). Every turn used tools and passed its `finish` check;
the resulting file was correct in all five:

| switch | turn 1 calls | turn 2 calls |
|---|---|---|
| claude-sonnet-5 → claude-opus-5 | write, shell, finish | read, edit, shell, finish |
| gpt-5.6-sol → gpt-5.6-luna | apply_patch (freeform), shell, finish | apply_patch (freeform), shell, finish |
| claude-sonnet-5 → gpt-5.6-sol | write, shell, finish | apply_patch (freeform), shell, finish |
| gpt-5.6-sol → claude-sonnet-5 | apply_patch (freeform), shell, finish | read, edit, shell, finish |
| gpt-5.6-sol → deepseek-v4.1-flash | apply_patch (freeform), shell, finish | read, edit, shell, finish |

So the Messages, Responses and Chat routes all accept a history holding calls to tools the new
environment does not declare, and the freeform `apply_patch` call travels as `{"input": …}` on
Messages and Chat. Offline: `cargo test -p p1-core --test reconfigure --test lead_resume_route`,
`cargo test -p p1-host --test lead_resume_decisions --test route_files`, conformance
`foreign_history_is_carried` on every adapter.
