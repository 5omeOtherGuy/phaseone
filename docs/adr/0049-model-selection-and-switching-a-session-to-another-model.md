---
adr: 49
title: Model selection and switching a session to another model
status: proposed
date: 2026-09-21
deciders: owner+lead
supersedes: []
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

- On acceptance this ADR supersedes ADR-0033 (`supersedes: [33]` is set then, with ADR-0033's
  `superseded_by`).
- A switch or a resume on another model can be refused, with a reason, before anything is
  sent or committed; it is never silently lossy beyond dropping foreign reasoning.
- Every adapter's `validate` must inspect history items, and conformance gains checks for
  foreign-origin histories.
- Stays `proposed` until the live acceptance of model-selection.md §3 passes on every shipped
  route family; a family that refuses a history shape is handled in its `validate` first.

## Alternatives considered

- Keep ADR-0033 and select only at start: leaves the owner's request half-done.
- Summarise the history on every switch: loses detail and costs a request per switch.
- Translate every foreign call to text: lossy for the common same-family case, and not needed
  if routes accept foreign call names (to be measured live).

## Evidence

To be recorded: the live switches of model-selection.md §3 (route, model, pass/fail, the
error code where one refused), and the offline tests of `reconfigure` and resume.
