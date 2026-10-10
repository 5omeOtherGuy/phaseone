---
adr: 170
title: Thinking off as an explicit reasoning effort
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0170: Thinking off as an explicit reasoning effort

## Context

Issue #641 (row R2 of #638) asks for MiMo's documented `thinking.type = "disabled"`.
ADR-0136 item 4 deferred reasoning off because the shared `Effort` enum had no off
variant and Chat had no off path; omission was never to mean off. ADR-0129 added
off on Responses only, as the native option `openai-responses.reasoning_enabled`.
The lead decided on #641 that off is a real effort choice: never sent as `low`,
never an omitted field, and refused by a route that cannot turn thinking off.

## Decision

1. `Effort` gains `Off`, ordered first. It travels everywhere an effort does: the
   module protocol (`WireEffort::Off`, `"off"` in `model-options.json`), profile
   `efforts` and `default_effort`, `--effort off`, `E/P:off`, and the `effort`
   enum of the `subagent` and `worker_start` tool schemas. An absent effort keeps
   meaning the route's default.
2. A model takes `off` only when its profile lists it (ADR-0039, data only); a
   `thinking = "budget"` profile carries no budget for it. The
   Chat adapter lowers `Off` to `thinking: {"type":"disabled"}` and sends no
   `reasoning_effort`, nor GLM's `clear_thinking` or Kimi's nested effort.
3. The Responses and Messages adapters refuse an explicit `Off` with
   `InvalidRequest`, even when the profile lists it. Responses keeps ADR-0129's
   native option as its only off.
4. The summary effort (#125) is the profile's lowest level other than `Off`, and
   `Off` only when the profile lists nothing else: listing `off` must not change
   how summaries are written while their quality without thinking is unmeasured.

## Consequences

No shipped profile lists `off`, so no shipped run changes. Using it on the MiMo
token plan takes a user profile edit (`efforts = ["off", …]`). Adding the enum
value is additive for module hosts and guests. The provider components must be
rebuilt to accept it. Whether thinking off helps task quality is unmeasured
(#638 open question 3).

## Alternatives considered

Map `Off` to `low` or omit the thinking field: rejected by the lead on #641.
A Chat-native boolean such as Responses' `reasoning_enabled`: a second spelling
of one choice, unreachable from `--effort` and the tool schemas. Building off on
Responses (`effort: "none"`) and DeepSeek Messages (`thinking: disabled`) as
well: outside #641; ADR-0129's option already covers Responses.

## Evidence

- MiMo openai-api page: `thinking.type` enabled/disabled, default enabled; `disabled`
  measured at 0 reasoning tokens (#638 first comment, R2 and S11).
- Tests: `p1-provider-openai-chat` `effort_off_*` (request.rs), `p1-provider-openai`
  and `p1-provider-anthropic` `effort_off_is_refused_*`, `p1-model-profile`
  `a_profile_lists_off_*`, `p1-module-protocol` fixture
  `model-options/effort-off.json`, `p1-host` `summary_effort` and `tests/models.rs`.
