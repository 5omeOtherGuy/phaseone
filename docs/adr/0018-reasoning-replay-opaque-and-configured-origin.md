---
adr: 18
title: Reasoning replay is opaque and keyed on the configured origin
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D17, docs/design/design-summary.md, docs/design/providers.md, docs/design/routes.md, docs/SLICE-REPORT.md, fd38d8f]
---
# ADR-0018: Reasoning replay is opaque and keyed on the configured origin

## Context

D17 (lead): native replay data is "opaque + versioned + tagged with its origin"
(`docs/design/design-summary.md` item 3). `routes.md` records the two incompatible shapes:
Claude sends thinking blocks back byte-exact with a signature; Codex sends
`encrypted_content`.

## Decision

`ReplayData` carries a version and an origin `{route, model}`. It is sent back byte-exact
only when its origin equals this provider's CONFIGURED route and model. Foreign reasoning is
dropped from the request, never downgraded to assistant text. The host decides whether a route
switch may proceed (`ResumeReport` reports dropped replay).

## Consequences

Replay survives providers that answer with dated model aliases, so tool use with thinking
keeps working on the next request. A model or route switch drops foreign reasoning from the request — reported by
`ResumeReport` on resume — instead of sending a request the provider would reject.

## Alternatives considered

Keying replay on the model name a response echoes. Both adapter briefs originally did
this; the shared conformance suite caught it on first contact (check 8,
`reasoning_replay_round_trips`), because providers answer with dated aliases. Converting
foreign reasoning into assistant text was also rejected.

## Evidence

`docs/design/providers.md` ("Origin is the CONFIGURED route + model ... Found by
conformance check 8 on the first adapter"). `docs/SLICE-REPORT.md` ("Three errors were the
LEAD's: ... replay keyed on the echoed response model in both adapter briefs — caught by the
shared conformance suite on first contact"). Fix: commit 65492cc (Anthropic adapter), repeated
for the Codex adapter before its first conformance run. Live, 2026-09-20: on the real coding
tasks both models produced reasoning blocks that were replayed across follow-up requests
without a rejected request (`docs/design/routes.md` §D, `docs/SLICE-REPORT.md`).
