---
adr: 12
title: Live provider checks are lead-only
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [D5, docs/SLICE-REPORT.md]
---
# ADR-0012: Live provider checks are lead-only

## Context

D5 (lead): the handoff's secrets and live-check rules require that authenticated traffic
never enters tests or fixtures. The live checks use the owner's existing logins and cost
subscription tokens.

## Decision

Live provider checks require `P1_LIVE=1`, are run only by the lead, and are not part of
the gate. The gate and all unit/conformance tests use hand-written fixtures and scripted
transports only.

## Consequences

The gate stays hermetic and fast, and no credential or authenticated traffic is stored.
Live coverage is a manual step that a merge does not wait for; route correctness is guarded
by the shared conformance suite (see ADR-0017).

## Alternatives considered

None recorded.

## Evidence

`docs/SLICE-REPORT.md` gives the command
`P1_LIVE=1 cargo test -p p1-live -- --nocapture --test-threads 1` and records the 2026-09-20
live run on both routes. Commit 2ba7db6 adds the lead-only live smoke checks.
