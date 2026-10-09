---
adr: 146
title: Route-scoped stream timeouts
status: accepted
date: 2026-10-09
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0146: Route-scoped stream timeouts

## Context

The owner selected issue #638 R6 (Native set) on 2026-10-09. p1 bounds HTTP
response headers at 120 s and silence between body chunks at 300 s globally.
MiMo Code uses a 480 s idle chunk bound, with a comment describing about five
minutes of cold-path silence for mimo-v2.5-pro on MiMo Router, not Flash on the
token plan. The benefit for the selected Flash route is unknown.

## Decision

Add optional top-level route keys `first_byte_timeout_secs` and
`stream_idle_timeout_secs`. Each accepts only integers in 30..=1800 inclusive.
Reject invalid values during route deserialization with an error naming the key;
zero does not disable a timeout. Omission independently keeps 120 s and 300 s.

Follow ADR-0137's native composition path: bound route -> `WasmProvider` ->
HTTP broker -> driver. No component settings or WIT change. The first-byte bound
covers response headers; the idle bound covers body reads, including non-2xx
classification and HTTP fallback from WebSocket. Any bytes, including SSE
comments, reset idle. Error messages report the effective value.

Keep connect at 30 s, WebSocket bounds unchanged, and preserve retry,
cancellation, authentication refresh and no-retry-after-visible-output semantics.
No shipped route or user configuration changes in this slice. The lead can set
the user MiMo subscription route to 480/480 after landing.

## Consequences

Slow HTTP/SSE providers can opt into longer waits without changing other routes.
The policy is route-wide across models and accounts. Both bounds remain finite;
longer per-attempt waits can extend the existing retry budget's wall time.
Native adapter defaults remain unchanged. Shared timeout messages still name
WebSocket's existing effective defaults.

## Alternatives considered

Changing global constants would change unselected routes. Per-model settings and
WIT changes put native transport policy into wire translation. Disabling bounds
would allow indefinite stalls and is not adopted from MiMo Code.

## Evidence

- Issue #638 R6 and lead brief `p1-lead-to-amp108-mimo-timeouts-20261009-1130.md`.
- MiMo Code at `6babeb0b`, `packages/cli/src/provider/provider.ts:44-50`:
  480,000 ms idle SSE timeout; no Xiaomi header timeout. Lead harness report §7
  records the cold-start comment's narrower model/host scope.
- Lead reports zero timeouts in twelve MiMo/other journals under
  `~/projects/rv-638/` (unchecked); effect remains unknown and tuning is opt-in.
- Paused-clock driver tests cover unequal shorter/longer bounds and effective
  messages; host tests cover integer/range validation, independent omission and
  parsed-route activation through the real component broker. Existing tests pin
  unchanged defaults and keep-alive resets. No live requests.
