---
adr: 33
title: A session resumes only on the route and model that recorded it
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/review-2026-09-20-dispositions.md, docs/design/seams.md, docs/design/journal.md, crates/p1-core/src/resume.rs]
---
# ADR-0033: A session resumes only on the route and model that recorded it

## Context

`seams.md §3` requires that continuing a conversation on an incompatible route is either
translated or explicitly rejected. The first slice did neither: on a changed origin the host
printed a warning, the adapter dropped the foreign reasoning replay, and the run went on. The
independent review of 2026-09-20 (departure D-B) pointed out that a warning makes this
visible but establishes nothing: the remaining transcript holds tool calls under another
environment's tool names and call shapes (function vs freeform), and whether a route accepts
such a history — even the same route with another model, once its reasoning items are gone —
was never tested. Same-origin resume, by contrast, is exercised live and by the crash tests.

## Decision

`Agent::resume` rejects a journal whose last `Environment` origin (route AND model) differs
from the assembled provider's: `ResumeError::RouteChanged { journalled, assembled }`, before
anything is committed. The rule lives in the core so that no host can continue such a session
by accident. `ResumeReport::route_changed` is removed — it can no longer be true. Every other
environment change (prompt, tools, options) still resumes and is re-committed as before.

## Consequences

- A user who wants another model starts a new session; the message says so and names both
  origins. Nothing is written to the session file by the refused attempt.
- A model alias that starts resolving to a different configured model id counts as a change.
  Origin is the CONFIGURED model (ADR-0018), so a provider-side re-pointing of the same id
  does not.
- Cross-route continuation becomes possible only through a deliberate compatibility policy
  (an injected decision plus history translation), which would supersede this ADR.

## Alternatives considered

- Keep warn-and-proceed: the behaviour under review; unproven and silently lossy.
- Allow a model change on the same route: plausible for Anthropic, unverified for the Codex
  route without its reasoning items; not worth a live experiment before it is needed.
- Decide in the host: a second host could forget it.

## Evidence

`cargo test -p p1-core --test lead_resume_route`; end to end
`cargo test -p p1-host --test lead_resume_decisions` (exit 1, both origins named, no request
sent, session file byte-identical).
