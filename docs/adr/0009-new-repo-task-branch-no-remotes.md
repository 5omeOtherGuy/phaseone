---
adr: 9
title: New repository on a task branch with no remotes
status: superseded
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: [10]
sources: [D2, D6]
---
# ADR-0009: New repository on a task branch with no remotes

## Context

D2 (lead, first slice): the repository was new, branch `main` held only the initial
scaffold commit, all work happened on task branch `slice-1`, and there were no remotes and no
pushes. D6 (owner) later superseded the "no remotes, no pushes" part: the project is public
and trunk-based.

## Decision

The original decision: no remotes, no pushes, no merge into `main` without the owner;
work on the task branch.

## Consequences

This is no longer in force. The public trunk-based workflow (see ADR-0010) replaced it;
the reason it was reversed was the owner's direction to develop in the open and to merge as
soon as the gate is green.

## Alternatives considered

None recorded.

## Evidence

D6 states it "supersedes D2's 'no remotes, no pushes'". The initial scaffold commit is
e6f41c0; `git log --format='%h %ad %s'` shows later work merged into `main`.
