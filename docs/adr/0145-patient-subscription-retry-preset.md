---
adr: 145
title: Patient subscription retry preset
status: accepted
date: 2026-10-09
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0145: Patient subscription retry preset

## Context

Subscription routes need to survive transient 429 bursts beyond the default's
three retries. This extends [ADR-0137](0137-route-scoped-deepseek-retry-policy.md).
Owner decision on 2026-10-09 at 11:15: "no per-vendor timing presets; at most
three presets (`default`, `deepseek`, `patient`)". The lead selected the shared
patient values from issue #637 R3 (ZCode), #638 R3 (MiMo Code), and #645 R7
(Kimi Code). Error classification remains per provider and is outside this slice.

## Decision

Add `patient` to the closed route `retry_policy` selector and select it on
`routes/glm-subscription.toml`. Omission still selects `default`.

| Preset | Retries | Base doubling | Cap | Additive jitter | Percentage jitter | Retry-After |
|---|---:|---|---|---|---|---|
| default | 3 | 2 s | 60 s | up to 250 ms | 0 | clamp to 240 s |
| deepseek | 5 | 500 ms | 10 s | 0 | ±10%, capped | exact through 10 s; reject longer |
| patient | 8 | 2 s | 32 s | 0 | ±10%, capped | exact through 300 s; reject longer |

Explicit hint limits admit hints inclusively before waiting and bypass the
default 4 × cap clamp. Otherwise a patient hint of 300 s would become 128 s.
The nominal patient schedule is 2, 4, 8, 16, 32, 32, 32, 32 s: 158 s total
(recomputed by summation), before jitter and excluding request time and hints.
No per-status caps, arbitrary numeric route knobs, classification changes,
loop-level retries, or new retryable statuses.

## Consequences

Selected routes can wait longer under transient pressure; a hint above five
minutes still surfaces the failure immediately. Default and DeepSeek behaviour,
cancellation, visible-output boundaries, and authentication refresh are unchanged.
The preset stays native transport policy, outside component settings and WIT.
Kimi's shipped route and user configurations are not changed; the lead owns
MiMo's user-route selection after merge.

## Alternatives considered

Separate ZCode, MiMo and Kimi presets were replaced by the owner's three-preset
limit. Changing the default would affect unrelated routes. Retaining the 4 × cap
clamp for explicit limits would violate exact admitted hints.

## Evidence

- Lead brief `p1-lead-to-amp105-retry-patient-20261009-1130.md` selects the values.
- Issue #637 R3: ZCode uses 10 retries, 2 s base, 60 s cap, and Retry-After
  through five minutes. Issue #638 R3: MiMo Code uses 2 s base, 30 s cap,
  and 429 waits through five minutes. Issue #645 R7: Kimi Code uses nine
  retries, 500 ms base, and 32 s cap. These are source inputs, not separate presets.
- Host route tests pin every patient field, GLM selection, and default three
  retries. The shared conformance declaration pins GLM to eight retries and
  asserts exactly nine attempts on exhaustion, following ADR-0137.
- Paused component-broker tests check eight retries, exponential jitter bounds,
  exact 300 s waits, and immediate failure without waits for 301 s hints.
