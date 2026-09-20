---
adr: 24
title: Authorization is Permit/Deny at the core; ask lives in the host
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D18, docs/design/seams.md, docs/design/assembly.md]
---
# ADR-0024: Authorization is Permit/Deny at the core; ask lives in the host

## Context

D18 (lead) and `seams.md` section 6: "Permit/deny/ask is an authorization outcome, not a
tool property that embeds UI." The host owns the interactive question; the core must stay
UI-free.

## Decision

The core's authorization outcome is exactly `Permit` or `Deny`. "Ask" is resolved inside
the host's policy implementation and reaches the tool only as a decision. The headless host
permits `ReadOnly` and denies everything else without `--yes`; the interactive prompt asks
`allow <tool> <summary>?`. The policy sees `{call, identity, effect}` and is asked only for
tools that exist and only when cancellation has not already fired.

## Consequences

No UI type enters the core. A headless run must have an explicit answer/deny policy
instead of waiting for a user, and `--yes` is all-or-nothing in this slice. General
authorization is separate from a tool's own safety invariants.

## Alternatives considered

Making "ask" a tool property or blocking the core on user input. `seams.md` section 6
rejects embedding UI in tools and requires a headless host to answer or deny explicitly.

## Evidence

`docs/design/core.md` section 4 (authorization row and the `Deny{reason}` case);
`docs/design/assembly.md` ("Authorization policy"). `docs/SLICE-REPORT.md` ("What is weak":
`--yes` is all-or-nothing; without it headless can only read).
