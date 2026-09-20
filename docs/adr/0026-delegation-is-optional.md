---
adr: 26
title: Delegation is an optional module, never imposed
status: accepted
date: 2026-09-20
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [D1, docs/design/pillars.md, docs/design/delegation.md, docs/SLICE-REPORT.md]
---
# ADR-0026: Delegation is an optional module, never imposed

## Context

Owner direction, recorded in `docs/design/pillars.md` as
"Orchestration: supported and optimised for, never imposed [owner, 2026-09-19]": agents may be
given tools to start other agents and are told when they finish, but there is "no mandatory
coordinating agent, no orchestrator mode the user must be in, and no delegation requirement."
D1 states "delegation optional". The lead specified the mechanism in `delegation.md`.

## Decision

Delegation is a tool module plus a worker service, assembled only when configured.
Completion is retained state plus ONE notification, so a missed wake-up loses nothing. A child
gets the task text only and its own assembled environment; one task owns a child; there is no
recursion (child environments are assembled without the delegation tools). `max_concurrent`
bounds running children. The host's cargo feature compiles the module out entirely.

## Consequences

Without the module the harness is a plain coding agent; a model that invents a
`worker_start` call gets `Unavailable` because the tool is not assembled. Children share the
parent's workspace in this slice (isolation is future work), and child usage is not summed
into the parent's totals.

## Alternatives considered

A mandatory coordinating agent or orchestrator mode (rejected by the owner in
`pillars.md`); automatic recursion into more workers (excluded by `delegation.md`).

## Evidence

`docs/SLICE-REPORT.md` acceptance 6: `cargo test -p p1-tool-delegate` covers
mid-turn/idle/blocked/wait notifications and a dropped notification; the live run
`p1 --env claude-delegating --yes "…start ONE worker in the gpt environment…"` exited 0; with
delegation compiled out 26 host tests are green and an environment naming `worker_start`
fails assembly with `UnknownToolModule`. Commit 5ccfb0b.
