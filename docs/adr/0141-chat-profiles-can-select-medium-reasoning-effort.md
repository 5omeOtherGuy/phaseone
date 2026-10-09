---
adr: 141
title: Chat profiles can select medium reasoning effort
status: accepted
date: 2026-10-09
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0141: Chat profiles can select medium reasoning effort

## Context

The owner selected MiMo row R1 in issue #638 on 2026-10-09 ("Native set").
The Chat adapter's composition check refuses profiles listing medium, and its
request builder has no medium wire encoding. Profile data alone cannot enable it.

## Decision

Allow medium in Chat profiles and encode a resolved Medium effort as
`reasoning_effort: "medium"` in both Chat dialects. Keep profile effort validation,
defaults and low/high/max encoding unchanged. ExtraHigh remains unsupported at
composition; its existing builder fallback is unchanged.

## Consequences

Profiles may opt into medium when their endpoint supports it. No shipped profile
bound on a Chat route lists medium, so shipped behavior does not change. This
slice changes no profile, route or environment file and makes no speed or quality
claim. MiMo user configuration is updated separately by the lead after merge.

## Alternatives considered

Profile-only enablement cannot pass composition or produce medium on the wire.
Vendor-specific effort handling is unnecessary: supported efforts remain profile
policy. Thinking off and ExtraHigh changes are outside selected row R1.

## Evidence

- Issue #638 records the lead's MiMo host probes accepting low/medium/high and
  rejecting max/minimal/xhigh; no live requests are repeated in this slice.
- `crates/p1-provider-openai-chat/tests/lowering.rs` checks explicit and default
  medium through public lowering in both dialects, unchanged low/high/max,
  high-only profile output and refusal of unlisted medium.
- `crates/p1-model-profile/src/lib.rs` keeps `resolve_effort` as the supported-effort
  check. Shipped Chat route bindings and profile effort lists were rechecked on
  2026-10-09; none lists medium.
