---
adr: 19
title: Usage fields kept distinct and unknown reported as unknown
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D17, docs/design/routes.md, docs/design/providers.md, docs/SLICE-REPORT.md]
---
# ADR-0019: Usage fields kept distinct and unknown reported as unknown

## Context

D17 (lead): all `Usage` fields are `Option` and kept distinct. `routes.md` records why:
the Claude route's `input_tokens` EXCLUDES cache reads and writes while the Codex route's
`input_tokens` INCLUDES cached tokens, and both are subscription routes with no per-request
price. `docs/SLICE-REPORT.md` records that the lead's adversarial tests found a missing
prompt-cache key.

## Decision

The reported usage keeps the route's fields distinct (`input_uncached`, `cache_read`,
`cache_write`, output, reasoning detail). Unknown values are `None` and printed as `?` or
`unknown`, never 0; `cost_micro_usd` is `None` on subscription routes. A per-agent
`prompt_cache_key` is sent where the route supports it (Codex).

## Consequences

Honest reporting survives routes with different token definitions instead of inventing a
common total. Consumers must handle `None`. The two routes' input totals are not directly
comparable, which the display makes visible (Claude shows cached separately).

## Alternatives considered

None recorded.

## Evidence

`docs/design/routes.md` usage sections and section C ("Input-token meaning"). The
measured table in `docs/SLICE-REPORT.md` shows the display
`in 34,395 (28,808 cached = 84 %)` on Claude and `in 15,620 (2,048 cached)` on GPT, with
`Cost unknown ... never 0`. `docs/design/providers.md` ("absent fields stay `None`").
