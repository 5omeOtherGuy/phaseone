---
adr: 62
title: A plan refusal is its own provider error kind; it is never refreshed or retried
status: accepted
date: 2026-09-24
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/providers.md]
---
# ADR-0062: A plan refusal is its own provider error kind; it is never refreshed or retried

## Context

The OpenCode Zen chat endpoint answers HTTP 403 for a model gated to OpenCode's own client,
with a valid key and a body shaped `{"error":{"type":"FreeTierError","message":"…"}}`
(issue #101). `kind_for_status` classified every 401/403 as `Authentication`, so the operator
was told the key was rejected and the shared driver ran a credential refresh that could only
fail — the same misdiagnosis ADR-0046 fixed for an account with no balance, through the same
allow-list mechanism, but a different fact: the plan does not include this model, the key is
fine.

## Decision

`ProviderErrorKind` gains `NotEntitled`. The chat adapter's `on_http_error` reads the same four
JSON positions and lookups as the no-balance check and looks the candidate words up in a second
fixed allow-list (`freetiererror`, `not_entitled`, `plan_not_allowed`); a hit emits
`NotEntitled` with the constant message
`the account's plan does not allow this model on this route`. The shared HTTP driver finishes at
once on this kind, exactly as on `InsufficientBalance` — no credential refresh, no adapter
retry — and an unknown 401/403 body keeps the status-based `Authentication` classification.

## Consequences

- The operator reads the right diagnosis (the plan, not the key) and p1 stops spending a refresh
  and a re-send on a request that cannot succeed.
- `p1-contracts` changes: a journal that records `NotEntitled` cannot be read by an older
  binary. The host's `transient_kind`/`retry_schedule` have catch-all arms; the TUI's
  `ProviderErrorKind` match is the one exhaustive site and gains an arm.
- The allow-list is a guess about wire shapes; an unknown shape falls back to today's behaviour,
  which is safe and merely unhelpful. The list grows on evidence.

## Alternatives considered

- Reuse `InsufficientBalance` with a different message: no interface change, but that kind's
  meaning ("the account has no balance") and ADR-0046's text would become false — a plan refusal
  is not an exhausted account — and AGENTS.md forbids editing an accepted ADR beyond its status.
- Fold the refusal into `Authentication`: keeps the bug (a pointless refresh masks the plan).

## Evidence

The live 403 body shape (OpenCode Zen, model gated to OpenCode's client, valid key) reported in
issue #101. Re-check: `cargo test -p p1-provider-openai-chat` (parser and `http_errors` tests)
and `cargo test -p p1-provider-http` (the driver test
`not_entitled_finishes_without_refresh_or_retry`).
