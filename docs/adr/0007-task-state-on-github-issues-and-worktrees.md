---
adr: 7
title: Task state on GitHub Issues and one worktree per task
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [D8, D13, scripts/new-worktree.sh, AGENTS.md]
---
# ADR-0007: Task state on GitHub Issues and one worktree per task

## Context

D8 (lead): several agents work at once, so task state needs one shared truth per concern.
D13 amends D8: `STATUS.md` stays, edited only by the lead, as the compaction-safe resume
record; `OWNER-QUESTIONS.md` is removed.

## Decision

Work items are GitHub Issues with labels `ready`, `in-progress`, `blocked` and `owner`.
Each task gets its own git worktree via `scripts/new-worktree.sh`. `STATUS.md` is written
only by the lead session; owner questions are issues labelled `owner`.

## Consequences

No file every agent rewrites is a contention point; `STATUS.md` has a single writer.
Claiming is visible to everyone on the issue tracker. Worktrees keep parallel agents out of
each other's checkout.

## Alternatives considered

Shared mutable files (`STATUS.md`/`OWNER-QUESTIONS.md`) as the task board — they
conflicted rather than informed (D8/D13).

## Evidence

`../../scripts/new-worktree.sh` creates `../phaseone-<slug>` on a fresh `task/<slug>`
branch. `AGENTS.md` ("Swarm protocol") states the labels and the single-writer rule. D13
records why `STATUS.md` survived.
