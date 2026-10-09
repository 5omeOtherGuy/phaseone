---
adr: 144
title: Z.ai reset hints interpret zone-less timestamps as UTC+8
status: accepted
date: 2026-10-09
deciders: lead
supersedes: [140]
superseded_by: []
sources: []
---
# ADR-0144: Z.ai reset hints interpret zone-less timestamps as UTC+8

## Context

Update 2026-10-09 to ADR-0140's timestamp assumption. Issue #657 records a
live exhausted-window error at 05:19Z: `next_flush_time` was `2026-10-09 13:40:00`,
while that window's quota endpoint reported reset at 05:40:00Z. The zone-less
value is UTC+8, not the operator's local time. The lead delegated the correction
under the owner's overnight delegation.

## Decision

Retain ADR-0140's stop-code classification, supported timestamp shapes,
sanitisation and reset-header precedence. Interpret a zone-less Z.ai timestamp
as UTC+8; an explicit `Z` remains UTC. Convert a future reset to the existing
`(resets in <duration>)` hint. Omit the hint for past or equal reset times,
invalid values and unsupported zones or fractions. Never print server prose.

Use the response's case-insensitive HTTP `Date` header (IMF-fixdate, GMT) as
the clock shared by native and component parsers. Without a usable `Date`,
native parsing falls back to the system clock; a component omits the hint.
Provider components have no linked clock capability and must not call
`SystemTime::now()`. No clock import, WIT change or new dependency is added.

## Consequences

Operators in any local time zone see the same remaining interval. Short-limit
retries, existing quota words and Kimi classification remain unchanged.
The component needs a usable response Date to show a body reset hint;
classification still stops immediately if it is missing. Relative time is
measured at response generation when Date is present, not after network transit.

## Alternatives considered

Printing a zone-less calendar time reproduced the reported ambiguity. Adding
a provider clock capability would require changing the synchronous runtime
linker outside this fix. Copying the raw server timestamp or assuming the
operator's zone would not satisfy the selected relative-hint behaviour.

## Evidence

- Issue #657: lead's live comparison of the error reset timestamp and quota
  endpoint reset for the same five-hour window. This task makes no live requests.
- `crates/p1-module-runtime/src/provider.rs`, `PROVIDER_LINKED`: provider
  components link transport and credential control, not clock.
- `cargo test -p p1-provider-openai-chat --test http_errors`: fixed response
  Date discriminates UTC+8 from UTC, verifies past/equal hint omission,
  calendar boundaries, header priority and sanitisation through scripted transport.
