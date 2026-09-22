---
adr: 51
title: Usage ledger: p1-usage module and p1 usage command
status: proposed
date: 2026-09-23
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0051: Usage ledger: p1-usage module and p1 usage command

## Context

Quota state must be visible for each configured route without leaking secrets or tying the TUI to provider APIs. See [usage design](../design/usage.md).

## Decision

Introduce `p1-usage` for snapshots, concurrent route probes and palette-independent grid rendering; expose it through `p1 usage`. The host supplies route metadata and p1-auth credential references.

## Consequences

The TUI can mount the ledger later without altering the module. Undocumented provider endpoints may change; probe failures remain visible as rows.

## Alternatives considered

Embedding endpoint calls directly in the host or TUI would duplicate the layout and couple it to transport.

## Evidence

`cargo test -p p1-usage --locked`; route kinds in `routes/*.toml`; endpoint fields in the read-only usage-meter script.
