---
adr: 5
title: Design baseline copied into the repository
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [D9, docs/design/README.md]
---
# ADR-0005: Design baseline copied into the repository

## Context

D9 (lead): the design baseline (`pillars.md`, `design-summary.md`, `seams.md`) was copied
into `docs/design/` so the public repo is self-contained. Reason recorded in D9: public
readers cannot follow links to the owner's workstation.

## Decision

The three baseline notes live in `docs/design/` as the public working proposal; the
owner's working copy outside the repo stays the source of the collaboration.

## Consequences

The public repo can be read on its own. The copies are a baseline, not the settled record;
`docs/design/README.md` now points at the ADRs for settled decisions, and the copies can
drift from the outside working copy.

## Alternatives considered

Link public documentation to the owner's workstation copy. D9 rejected this because
public readers cannot follow the links.

## Evidence

`docs/design/pillars.md`, `docs/design/design-summary.md` and `docs/design/seams.md`
exist in the repository; commit 99ee941 ("Initial scaffold: workspace, gate, design
baseline, swarm workflow").
