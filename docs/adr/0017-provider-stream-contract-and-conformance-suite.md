---
adr: 17
title: One terminal stream event and one shared conformance suite
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D17, docs/design/providers.md, docs/design/routes.md, docs/design/seams.md]
---
# ADR-0017: One terminal stream event and one shared conformance suite

## Context

D17 (lead). `seams.md` section 3 requires "one terminal outcome: completed, failed, or
cancelled" and that "partial calls are never executed". Instead of one test suite per adapter,
`providers.md` specifies one suite shared by both subscription adapters.

## Decision

A provider stream emits exactly one terminal `Finished` event; an EOF without one is a
failure, never an implicit completion. Tool calls are surfaced only when complete; partial
calls are never executed or stored. Unknown usage stays `None`, never zero. The shared
`p1-provider-conformance` suite runs the same 15 checks for every adapter, and each check has
a seeded-bug self-test in that crate.

## Consequences

Both adapters are held to identical ordering, terminal, cancellation, error and usage
checks, so a route-specific bug cannot hide behind route-specific tests. Route-native shapes
are covered by hand-written fixtures. The suite proves itself by failing on seeded bugs.

## Alternatives considered

None recorded.

## Evidence

`docs/SLICE-REPORT.md` acceptance 4: `cargo test -p p1-provider-anthropic --test conformance`
and the same for `p1-provider-openai` run the same 15 checks; `cargo test -p p1-provider-conformance`
runs one seeded bug per check. Commit fd38d8f added the suite; f07a389 added the shared HTTP
helper it runs on.
