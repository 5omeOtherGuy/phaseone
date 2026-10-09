---
adr: 147
title: Z.ai Messages effort lowering and Flash subscription binding
status: accepted
date: 2026-10-09
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0147: Z.ai Messages effort lowering and Flash subscription binding

## Context

Owner selected R5 and R9 of issue #637 on 2026-10-09 ("Native set").
ZCode's Coding Plan wire is Anthropic Messages with enabled thinking and
`output_config.effort`. p1 only offered GLM-5.3 on Z.ai Chat, while Flash was
restricted to ClinePass by the owner's 2026-09-24 decision.

## Decision

One `zai` account serves `glm-subscription` and new `glm-messages`, retaining
`store_id = "glm-subscription"`, `ZAI_API_KEY`, store-only lookup and the old
Chat replay origin. Both routes bind lowercase `glm-5.3` and `glm-5.3-flash`.
The Flash profile states the documented 1M context and 131,072 output ceiling,
low/high/max efforts, default high as ClinePass Flash revision 2.

Messages uses endpoint base `https://api.z.ai/api/anthropic`, to which the adapter
appends `/v1/messages`. `[adapter_settings] dialect = "zai"` (ADR-0139 §8) lowers
enabled or preserved thinking profiles to `thinking: {type: "enabled"}` and
the resolved effort to `output_config.effort`. It never sends disabled thinking,
`clear_thinking`, Claude identity/cache markers/betas or OpenCode session headers.
Authentication is `x-api-key` plus `anthropic-version: 2023-06-01`.
The existing broker also attaches its Bearer header, as on OpenCode Go Messages;
no new credential scheme or broker setting is introduced.

R5 reverses "Flash only on ClinePass" for GLM Flash on 2026-10-09; the ClinePass
binding remains. New `glm-messages` environment copies `glm` with only its route
changed and a byte-identical prompt. Existing environment defaults stay unchanged.

## Consequences

`--model glm/glm-5.3-flash` selects Flash on Chat; `--model glm-messages/glm-5.3`
and `--model glm-messages/glm-5.3-flash` select Messages on the same account.
All accept `:low`, `:high`, `:max` and `@zai`. Bare GLM profile references outside
these environments are now ambiguous and need the environment/profile form.

The reported "about 3x fewer credits" for Flash is doc-derived from summaries,
unchecked, not a p1 measurement. Messages speed/cache effects and its full context
window remain unmeasured; the environment carries `glm`'s existing policy. No live
model requests are part of this implementation.

## Alternatives considered

Reuse OpenCode Go dialect: rejected because it requires enabled-only profiles,
can send disabled thinking and adds an OpenCode session header. Duplicate accounts
per wire: rejected by ADR-0139. An environment alias cannot change a route, so a
full environment follows the DeepSeek Messages precedent (ADR-0134/0138).

## Evidence

Issue #637 comparison R5/R9 and lane A `glm/report-api.md`: one Flash request
answered on the key; one Messages probe returned signed thinking and
`cache_read_input_tokens`. ZCode v3.14.4 model configuration and client code were
read, not run. Capacity/effort source: docs.z.ai/guides/llm/glm-5.3-flash.

`crates/p1-provider-anthropic/tests/glm_messages.rs` checks both shipped profiles,
all supported efforts, omission, exact body and headers. `crates/p1-host/tests/accounts.rs`
checks both wires read a fake legacy store entry, the old Chat origin and prompt
identity. Model and route inventories pin the new rows. All tests use scratch data
and in-process lowering, with no live network.
