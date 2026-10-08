---
adr: 133
title: Endpoint approval for store-only OAuth routes
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/adr/0110-credential-sources-are-bound-to-endpoint-origins.md]
---
# ADR-0133: Endpoint approval for store-only OAuth routes

## Context

The retained live Librarian reasoning-off check needs a fresh Codex usage probe.
ADR-0110 requires explicit probe-origin approval for store-only OAuth routes, but
the CLI's metadata-only approval command accepts only API-key routes. Ordinary
login cannot approve an existing Codex OAuth grant without an import/flow that
p1 does not provide. The owner answered "Yes please" to repairing the OAuth
endpoint-approval path so the owner can authorize the live check.

## Decision

Extend `p1 login <route> --trust-endpoint` to `store_only` Claude Code and Codex
OAuth routes, retaining API-key support. Reuse the locked origin-metadata writer
without opening, importing, resolving or replacing a credential document or
reading stdin. Recover interrupted origin metadata, but leave interrupted
credential recovery files untouched. Reject borrowed OAuth routes and credential
kind `none`.

## Consequences

An operator can approve the configured endpoint origin for an existing stored
OAuth grant, including same-origin usage probes. Approval does not prove a grant
exists, is valid, or supports the chosen model. Ordinary OAuth login remains a
usage error; this adds neither an OAuth flow nor an account switch.

Compiled shipped-origin checks, borrowing restrictions, per-access/refresh origin
checks and protected metadata writes remain unchanged. Different usage origins
do not inherit approval. Executing approval against the real store remains an
explicit owner-authorized action, separate from implementing this command.

## Alternatives considered

Automatic trust during usage or inference would let a route authorize itself.
Re-importing a grant would unnecessarily read and replace credential data.
Approving borrowed OAuth would obscure its compiled same-kind destination ceiling.

## Evidence

`crates/p1-host/tests/credential_origins.rs` checks both store-only OAuth kinds
with an intentionally unparseable credential document, unchanged document bytes,
unchanged credential recovery bytes, recovered metadata for another route,
unconsumed stdin, exact origin approval and protected metadata mode. Refusal
cases cover both borrowed OAuth kinds and `none`, preserving prior approval.
Existing ordinary-login and store-only tests retain their OAuth refusal cases.

Run `cargo test -p p1-host --test credential_origins` and
`cargo test -p p1-host --test login --test store_only` offline. These tests do not
establish live server acceptance of Librarian's reasoning-off setting.
