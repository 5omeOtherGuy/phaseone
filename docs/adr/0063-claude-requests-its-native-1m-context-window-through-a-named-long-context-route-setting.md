---
adr: 63
title: Claude requests its native 1M context window through a named long_context route setting
status: proposed
date: 2026-09-24
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/routes.md, docs/design/context.md]
---
# ADR-0063: Claude requests its native 1M context window through a named long_context route setting

## Context

The leads run Opus 5.5 through p1 on the Claude Code subscription. The `claude` environment capped them at a 200k window and summarized at 120k, which compacted a lead about every 40 requests (usage audit of the two lead journals, #125). The Messages adapter sent no beta that lifts the 200k default. Owner (2026-09-25): "as an immediate hotfix giving Opus 5.5 at least 500k or the native 1m context window", and "I know it works" for the subscription.

## Decision

The `anthropic-messages` adapter gains a named route setting `long_context` (default off). When a route enables it, every request carries the `context-1m-2025-08-07` beta. `routes/anthropic-subscription.toml` enables it, and `environments/claude` moves to a 1,000,000-token window, summarizing at 500,000.

## Consequences

- Leads on Claude compact far less often; summaries keep their 12k cap (#125 retunes the summarizer).
- The setting is named by behaviour like `account`, never a free-form header, and it is off for every other route and test.
- Deploy coupling: an older binary rejects the new key (`deny_unknown_fields`), and a 1M environment without the beta fails requests above 200k. So the binary, `routes/anthropic-subscription.toml` and `environments/claude` deploy together.
- Requests with large contexts consume more subscription quota per turn, even from cache.

## Alternatives considered

- Derive the beta from the agent's context window (the #113 role-window plumbing): more code, deferred with #113.
- Always send the beta for the subscription account: this hides a capacity choice inside the adapter.
- Stay at 200k: rejected by the owner.

## Evidence

- `crates/p1-provider-anthropic/tests/request.rs::the_long_context_beta_joins_every_other_beta`.
- `tests/stream.rs::a_long_context_route_sends_the_1m_context_beta_and_a_default_route_does_not`.
- `crates/p1-host/tests/anthropic_route.rs`: the shipped route parses with `long_context = true`, and the shipped claude environment pins 1,000,000/500,000.
- Live: the restarted leads (2026-09-25).
