---
adr: 137
title: Route-scoped DeepSeek retry policy
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0137: Route-scoped DeepSeek retry policy

## Context

The owner selected R3 of issue #622 on 2026-10-08. The lead's implementation
brief specifies dsh's policy for DeepSeek routes, preserving today's default
for every other route. Components perform wire translation; retry is native
transport policy, not a guest setting.

## Decision

Add optional top-level route `retry_policy`, a closed preset selector: `default`
(also omission) or `deepseek`. The host composes it with `WasmProvider::with_retry`.
No WIT, credential, status-classification or provider-wire change.

Select `deepseek` on the four existing OpenCode Go routes and both ClinePass
routes: five retries after the initial attempt, 500 ms base doubling, 10 s cap
after symmetric ±10% multiplicative jitter. Honour parsed Retry-After exactly
through 10 s inclusive; a longer hint surfaces the original failure without
waiting or shortening the hint. Cline's GLM model shares this route-wide policy.
This is a deviation from "DeepSeek routes only": the lead confirmed keeping
the shared ClinePass route policy on 2026-10-08 rather than splitting its GLM binding.

The default retains three retries, 2 s base, 60 s cap, additive jitter up to
250 ms and the existing Retry-After clamp at four times the cap. Keep the existing
deterministic policy-seeded jitter source; match dsh's backoff range and cap,
not its Math.random entropy source. No extra dependency.

## Consequences

DeepSeek starts retrying sooner and can spend five retries, but stops immediately
when a server asks for more than 10 s. Other routes and native adapter defaults
are unchanged. Existing cancellation, no-retry-after-visible-output, account
diagnosis and authentication-refresh boundaries remain in the shared driver.
RetryPolicy gains percentage jitter and an optional server-hint limit; the broker
checks admission before calculating a delay. The selected preset is native-only.

## Alternatives considered

Changing the global default would alter unselected routes. Adapter settings or
WIT changes would put transport policy in wire translation. Shortening longer
server hints would contradict the owner's requested error-surfacing policy.
Arbitrary route numeric knobs are unnecessary for these two selected presets.

## Evidence

- Issue #622 Phase 1 comparison, R3, and the lead implementation brief
  `p1-lead-to-amp98-622-r1-r4-r3-20261008-2002.md` pin the policy.
- Installed `@deepseek-ai/dsh@0.2.0-rc.2`,
  `~/.local/share/dsh/node_modules/@deepseek-ai/dsh-llm-retry/lib/index.js`:
  default settings, `localDelay` and Retry-After handling.
- Retry unit tests discriminate positive and negative percentage jitter, doubling,
  final cap and hint boundaries; paused driver tests exercise exactly six attempts,
  10 s exact waiting and 11 s immediate error without a Wait event.
- Host route tests pin the six presets and retain defaults everywhere else;
  provider activation tests exercise the selected policy through the real component
  broker over scripted transport, contrasting five vs three retries and long hints.
