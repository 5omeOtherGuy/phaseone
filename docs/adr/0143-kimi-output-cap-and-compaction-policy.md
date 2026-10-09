---
adr: 143
title: Kimi output cap and compaction policy
status: accepted
date: 2026-10-09
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0143: Kimi output cap and compaction policy

## Context

Issue #645 compares Kimi Code with p1. The lead selected R5 and R6 on the owner's
delegation on 2026-10-09. The kimi environment sent no output cap on agent turns,
so the server's default decided the response size, and it used the 256K floor
(window 262144, reserve 32000, summarize 150000, no trim) because the plan tier
was unknown. The tier has since been measured. Extend ADR-0136's compaction rule
to kimi, as ADR-0142 does for GLM.

## Decision

1. The kimi environment sends `max_output_tokens = 131072` on every agent turn,
   equal to the K3 output default and to `profiles/kimi-k3.toml`'s ceiling.
2. Window 1048576, reserve 131072 (the cap). Summarize and trim together at
   min(0.8 * W, W - O - 65536) = min(838860, 851968) = 838860 (0.8 * W floored).
   Keep recent 50000, user verbatim 8000 and summary cap 12000 unchanged. Retain
   ADR-0136's trim, re-measure, summarize-if-still-above sequence.
3. The profile states no `context_tokens`: the window is tier-dependent and stays
   in the environment. No route, wire model, thinking or effort changes.

## Consequences

Histories below 838860 tokens stay byte-exact instead of being rewritten at
150000, so more of the cached prefix survives. The input wall is 917504. Every
agent turn is capped at the reserve, so a response cannot outgrow the room the
reserve keeps. If the account drops to a Moderato / Plus plan (256K), requests
above 262144 tokens fail with 401 until the environment is changed back; the
`k3-256k` binding is separate work (#646). The documented 2 MB request-body
limit may bind before the token window on a 1M history (inferred; bytes per
token unmeasured). The adapter sends the cap as Chat `max_tokens`, which the
Kimi Open Platform page calls deprecated in favour of `max_completion_tokens`;
the wire spelling is not changed here. Other environments retain their settings.

## Alternatives considered

Keep the 256K floor: rejected after the tier measurement. Kimi Code's compaction
point max(0.85 * W, W - 50000) (891290 at 1M): rejected because it keeps no
output reserve in p1. ADR-0142's "reserve = profile ceiling" at 256K would leave
65536 of pressure room and is not needed on the 1M tier. Route changes, `k3-256k`,
error mapping and the thinking spelling are other #645 rows.

## Evidence

- www.kimi.com/code/docs/en/kimi-code/models.html: k3 context 1048576 for
  Allegretto / Pro and above, 256K on Moderato / Plus; Kimi Open Platform docs:
  K3 `max_completion_tokens` default 131072.
- Live `GET https://api.kimi.ai/coding/v1/models` (lead, issue #645): k3
  `context_length` 1048576. Lead measurement 2026-10-09: a 274,765-token `k3`
  prompt returned HTTP 200, while a Moderato / Plus account answers 401 above
  262144, so the account has the 1M tier.
- Kimi Code `agent/fullCompaction/strategy.ts:18-28` (compaction point) and
  `trait.ts:69-76,105-109` (always sends `max_completion_tokens`), per issue #645.
- Offline inventory `crates/p1-assembly/tests/context_table.rs` pins window,
  reserve, threshold, trim and the cap; built `p1 env show kimi`.
