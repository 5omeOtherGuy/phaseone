---
adr: 11
title: The gate is the single definition of green
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [D7, D19, scripts/gate.sh, .github/workflows/ci.yml]
---
# ADR-0011: The gate is the single definition of green

## Context

D7 (lead): CI must not drift from the local gate, so there is one job that runs
`scripts/gate.sh`. D19 (lead) adds the other half: `main` must always be green, so red-first
acceptance suites live on the implementing task branch, never on `main`.

## Decision

CI is one job running `scripts/gate.sh` on pushes to `main` and on pull requests. There
are no other workflows, no matrices and no separate review/audit bots. Red-first suites are
committed on the implementing task branch; `main` always has a green gate.

## Consequences

One definition of green cannot drift between laptop and CI. A red suite reaching `main`
makes every other worker's "gate green" criterion unreachable and turns CI red; this actually
happened on 2026-09-20 and was corrected one commit later (D19).

## Alternatives considered

Separate review/audit workflows or build matrices (D7 rejected them); keeping a red-first
suite on `main` (D19 rejected it).

## Evidence

`../../.github/workflows/ci.yml` has one `gate` job whose only build step is
`scripts/gate.sh`. Commits ced8fd2 ("Keep red-first acceptance suites off main until the core
is implemented") and 106b8a4 ("Record D19").
