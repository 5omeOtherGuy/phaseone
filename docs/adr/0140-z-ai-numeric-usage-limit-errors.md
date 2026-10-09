---
adr: 140
title: Z.ai numeric usage-limit errors
status: superseded
date: 2026-10-09
deciders: owner
supersedes: []
superseded_by: [144]
sources: []
---
# ADR-0140: Z.ai numeric usage-limit errors

## Context

The owner selected the “Native set” in issue #637 on 2026-10-09, including
R4: distinguish Z.ai's exhausted allowance from short-lived HTTP 429 limits.
The chat adapter already stops on fixed quota words, but numeric string codes
currently fall through to the ordinary rate-limit retry budget.

## Decision

On HTTP 402/429, classify exact string codes at `/error/code` as
`UsageLimitExhausted`: 1304, 1308, 1309, 1310, 1311, 1313, and 1316–1321.
Keep 1302/1303/1305/1312 and unknown codes on their existing status-based path.
Do not change the existing quota-word allow-list, route policy or retry budget.

Keep the fixed usage-limit message and existing reset-header precedence.
Without a header hint, extract only a validated timestamp after the documented
“reset at” / “Resets at” wording, or an explicit `next_flush_time` marker,
from `/error/message` of a stop-code body. Display `(resets at <time>)`,
never provider prose. Accept `YYYY-MM-DD HH:MM:SS` or a `T` separator with
optional `Z`; validate calendar dates and clock ranges. Unsupported time formats,
including fractional seconds and numeric offsets, omit the hint rather than
guessing the reset or dropping a timezone.

## Consequences

A used-up Z.ai allowance ends on the first response, without refresh or retry.
Other error shapes, statuses and named quota words retain their behaviour.
The docs do not specify the format of `next_flush_time`, and no live 429 body
was captured: the accepted calendar timestamp formats are an explicit assumption.
Expired-plan and model-entitlement codes 1309/1311 use the same terminal
usage-limit diagnosis as required by the selected split, despite different causes.

## Alternatives considered

Matching message prose for classification risks misclassifying other providers.
Changing the global retry policy affects unrelated routes. Copying server text
would break the adapter's sanitisation contract. A date/time dependency is not
needed for bounded timestamp validation and absolute-time display.

## Evidence

- https://docs.z.ai/api-reference/api-code, re-fetched on 2026-10-09:
  1302/1305 short limits; 1308/1310 and 1316–1321 reset-at messages;
  1309 expiration, 1311 entitlement and 1313 fair usage. 1303/1304/1312
  are absent from the page, not contradictory to the selected harness policy.
- ZCode v3.14.4, local donor
  `~/.agents/xo/dispatch/p1-lead-20261004/glm/harness-src/deb/opt/ZCode/resources/glm/zcode.cjs`,
  line 2068: retryable 1302/1303/1305/1312; terminal 1304/1308/1309/1310/1311/1313.
  Conceptual adoption only; none of its retry machinery copied.
- `cargo test -p p1-provider-openai-chat --test http_errors`: scripted responses
  discriminate first-response stops, unchanged retry budgets, exact shape/status
  matching, valid reset timestamps and malformed hints without leaking prose.
