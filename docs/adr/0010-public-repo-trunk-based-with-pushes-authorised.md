---
adr: 10
title: Public repository, trunk-based, pushes and merges authorised
status: accepted
date: 2026-09-19
deciders: owner
supersedes: [9]
superseded_by: []
sources: [D6, D11, AGENTS.md, README.md]
---
# ADR-0010: Public repository, trunk-based, pushes and merges authorised

## Context

Owner decisions D6 and D11 (2026-09-19). D6: develop p1 in the open as a public GitHub
repo (`5omeOtherGuy/phaseone`), trunk-based, short-lived `task/*` branches, no review gate and
no branch protection. D11 records the owner's words: "disregard the handoff when it comes to
no push no merge, push and merge, this is your project."

## Decision

`main` is the trunk. Work happens on short-lived `task/*` branches created by
`scripts/new-worktree.sh` and is merged as soon as `scripts/gate.sh` is green; the lead may
push and merge. D2's "no remotes, no pushes" is superseded. Still not authorised: new
billing, purchased capacity, publishing credentials or private prompts (D11).

## Consequences

Several agents can land work without a review bottleneck, and the public can read the
history. The cost is that `main` has no protection: the gate must stay green (see ADR-0011).
Task branches are the only place a red-first suite may live.

## Alternatives considered

Continuing without remotes and without pushes, as D2 said. The owner reversed that
because the repository should be optimised for rapid parallel development (D6).

## Evidence

D11 quote above. Commit 59e21c7 ("Share one cargo target dir across worktrees; record
push/merge authority"). `AGENTS.md` ("Swarm protocol", "Trunk-based") and `README.md`
("Working here") state the workflow.
