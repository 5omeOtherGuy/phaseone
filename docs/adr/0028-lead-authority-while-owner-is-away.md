---
adr: 28
title: Lead authority while the owner is away
status: accepted
date: 2026-09-20
deciders: owner
supersedes: []
superseded_by: []
sources: [D16]
---
# ADR-0028: Lead authority while the owner is away

## Context

Owner decision D16 (owner message, 2026-09-20): while the owner is away, the lead has
full authority for the project and is not to stop for questions.

## Decision

The lead proceeds without waiting for owner answers. An action blocked by the lead
harness's permission classifier may be handed to a deepseek worker; the lead does not use that
route around safety-relevant blocks (destructive operations, credentials).

## Consequences

Unattended progress is possible, and there is still an explicit boundary around
destructive and credential-touching actions. Decisions made under this authority are still
recorded (D16 and the ADRs).

## Alternatives considered

None recorded.

## Evidence

D16 records the owner message and names the one permitted workaround (a deepseek worker
for harness-permission blocks) and its limit.
