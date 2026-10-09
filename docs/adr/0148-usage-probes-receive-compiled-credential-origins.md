---
adr: 148
title: Usage probes receive compiled credential origins
status: accepted
date: 2026-10-09
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0148: Usage probes receive compiled credential origins

## Context

ADR-0139 approves shipped credential origins at compile time, but usage probes
still require stored approval for every API-key account. GLM and OpenCode Go
therefore appear unsupported even when their probe origin is compiled as trusted.
The lead confirmed the correction on 2026-10-09 for issue #637 R10.

## Decision

Add optional compiled origins to host-provided `UsageRoute` metadata. The host
derives them only from its existing compiled trust anchor, intersecting anchors
for account id and store identity when both exist. Account-file declarations
cannot provide these approvals. A compiled identity may probe only these origins,
without requiring an additional store approval. Other identities retain protected
store approval per probe origin and the existing borrowed-credential restrictions.

Classify a GLM HTTP-success body with `success: false` as a credential failure
with fixed text, never the server's explanation.

## Consequences

Usage follows the same shipped trust boundary as provider calls. A custom
account naming the same probe remains unapproved, and chat approval cannot
authorize a different usage host. Probe URLs, polling and window mapping stay
unchanged. The metadata field changes the Rust interface, not the CLI or WIT.

## Alternatives considered

Hard-coding another route-id exemption would duplicate the host's trust table
and fail for separated accounts. Treating declared origins or a probe name as
approval would let a user file approve itself.

## Evidence

`crates/p1-host/src/usage.rs` assembles usage metadata;
`crates/p1-host/src/routes.rs::shipped_origins` owns compiled approvals;
`crates/p1-usage/src/probe.rs::check_probe_origin` enforces probe trust.
Regression tests cover inline GLM, named GLM, custom approval and refusal,
and refused GLM response bodies. The shipped-component conformance test covers
Z.ai stop errors without a usable HTTP Date, completing PR #659's retained P2.
