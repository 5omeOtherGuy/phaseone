---
adr: 1
title: p1 is a new project; Iris is a parts donor
status: accepted
date: 2026-09-19
deciders: owner
supersedes: []
superseded_by: []
sources: [D1, LICENSE, docs/design/pillars.md, docs/design/design-summary.md, docs/SLICE-REPORT.md]
---
# ADR-0001: p1 is a new project; Iris is a parts donor

## Context

Owner decision (D1, 2026-09-19). The ledger records the owner's direction: "p1 is a new
project; Iris is a parts donor." `pillars.md` (pillar 3) states the same direction: Iris
"donates parts (Nexus as raw material with a reduced interface), not structure."

## Decision

p1 starts as a new repository with its own contracts and module boundaries. Iris code is
copied into p1, adapted to p1's contracts, and never depended on; the donor path is named in
the commit message.

## Consequences

Iris logic that fits p1 is reused quickly; everything p1 does not want (UI, the `wayland`
tier, compaction, login flows, bash sessions and sandbox, fuzzy edit matching, mythology
names) stays behind. Donor code needs adapting where the contracts differ.

## Alternatives considered

Refactor Iris in place. `docs/design/design-summary.md` explicitly chose "a minimal new
loop with Send interfaces and representative donor components (not a wholesale Iris
conversion)".

## Evidence

`docs/SLICE-REPORT.md` lists what was copied and what was left behind; 21 crates, about
33k lines of Rust, of which roughly half are tests. `LICENSE` is copied from the donor.
