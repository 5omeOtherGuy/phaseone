---
adr: 202
title: An OpenAI Chat Completions dialect and core route; both OpenAI routes send store false
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/873, https://github.com/5omeOtherGuy/phaseone/issues/847, https://github.com/5omeOtherGuy/phaseone/issues/840, https://github.com/5omeOtherGuy/phaseone/issues/766, docs/adr/0197-api-key-dialects-and-pay-as-you-go-core-routes-the-default-environment-is-default.md, docs/adr/0170-thinking-off-as-an-explicit-reasoning-effort.md, docs/design/routes-and-profiles.md, https://developers.openai.com/api/docs/guides/reasoning, https://developers.openai.com/api/docs/guides/function-calling, https://developers.openai.com/api/docs/models/gpt-6-sol, https://developers.openai.com/api/docs/models/gpt-6-luna, https://developers.openai.com/api/docs/models/gpt-6-astra, https://developers.openai.com/api/docs/models/gpt-5.6-sol, https://developers.openai.com/api/docs/models/gpt-5.6-terra, https://developers.openai.com/api/docs/models/gpt-5.6-luna, https://developers.openai.com/api/docs/models/gpt-5.5, https://github.com/openai/openai-python/blob/main/src/openai/types/chat/completion_create_params.py]
---
# ADR-0202: An OpenAI Chat Completions dialect and core route; both OpenAI routes send store false

## Context

ADR-0197 (#847) gave the core two pay-as-you-go routes and left two of its plan's points open,
which the owner wants closed (#873):

- The plan named a third core route, `openai-chat` on `api.openai.com/v1/chat/completions`.
  None shipped, because the chat adapter could serve no shipped GPT profile: every one is
  `thinking = "effort-level"`, which neither chat dialect encodes, and both dialects send
  `thinking` and `reasoning_content`, fields OpenAI's Chat Completions API does not define.
- `routes/openai.toml` sent no `store`, so the API kept responses for its retention period,
  which the Codex subscription route never allowed (`store: false`).

The Chat Completions API (reference and SDK types, fetched 2026-10-11) takes `reasoning_effort`
with `none`, `minimal`, `low`, `medium`, `high`, `xhigh` and `max` (model-dependent; the default
is `medium` on every bound model), function tools as `{"type": "function", "function": {name,
description, parameters}}`, tool results as `role: "tool"` messages with `tool_call_id`,
streamed calls as `delta.tool_calls[].index/id/function.name/function.arguments`, the output
cap as `max_completion_tokens` (`max_tokens` is deprecated and counts no reasoning tokens),
`prompt_cache_key` and `store` in the body. It returns no reasoning content and defines no
`reasoning_content` or `reasoning` on an assistant message. The model pages say GPT-6 Sol and
GPT-6 Luna support "function calling only with `reasoning_effort` set to `none`" on Chat
Completions, and the reasoning guide says "Chat Completions does not support function calling
with GPT-6 Astra or GPT-6.1 Sol" (Astra also rejects `none`). The GPT-5.6 Sol, Terra and Luna
and GPT-5.5 pages list Chat Completions and function calling without such a restriction.

## Decision

1. **Dialect `effort-level` on `openai-chat`**: the public Chat Completions encoding. The
   resolved effort goes out as `reasoning_effort` (`off` as `none`, `extra_high` as `xhigh`,
   the others by name); an effort-less request carries no reasoning field, so the API's
   default applies, as on the Responses adapter. No `thinking` object and no
   `reasoning_content` on any message, so an assistant message is `content` plus `tool_calls`;
   a reasoning block in the history, own or foreign, is dropped. The output cap is
   `max_completion_tokens`, the request's cache key is the body's `prompt_cache_key` (the
   route needs no session header), and every body says `store: false`. Function tools keep the
   adapter's nested shape; the stream parser is unchanged. Only a `thinking = "effort-level"`
   profile composes with this dialect, and it needs no `default_effort`; the thinking-mode
   dialects keep refusing `effort-level` profiles. The dialect is named after the one policy it
   encodes, as the Responses adapter describes its own lowering.
2. **`routes/openai-chat.toml`**: `https://api.openai.com/v1/chat/completions`, an inline
   `api-key` credential on `OPENAI_API_KEY` (the same variable as `routes/openai.toml`, an
   implicit account of its own), dialect `effort-level`, binding `gpt-5.6-sol`,
   `gpt-5.6-terra`, `gpt-5.6-luna` and `gpt-5.5`. Not bound: `gpt-6-sol`, `gpt-6-luna` (tool
   calls only without reasoning, and the profiles list no `off`) and `gpt-6-astra` (no tool
   calls on this API). The route file says why, and the Responses route stays the one to prefer:
   it replays encrypted reasoning across tool turns, which this wire cannot.
3. **`store: false` on every Responses body**, the API-key account included, with the encrypted
   reasoning replay (`include: ["reasoning.encrypted_content"]`) the subscription lane already
   uses. The account-dependent `store` of ADR-0197 decision 2 is reversed; the rest of that
   record stands, as does its decision 3 apart from the chat route it declined.
4. **The conformance suite gains `run_all_without_reasoning_replay`**: every check of `run_all`
   except the reasoning replay round trip, for a wire that returns no reasoning. `run_all`
   itself is unchanged, and the shipped thinking-mode routes keep running it.

## Consequences

- An OpenAI API key reaches the GPT-5.x models over Chat Completions through a user
  environment that names `openai-chat`; a gateway that speaks only that protocol can be
  reached by a user route file with the same dialect. The shipped environments still name
  neither OpenAI route, so `p1 models` lists no new row.
- Neither OpenAI route stores responses server-side.
- Two routes read `OPENAI_API_KEY`; `p1 login openai` and `p1 login openai-chat` store a key
  per route. A shared account file would be the alternative if that proves awkward.
- A reasoning-capable Chat Completions request reasons server-side and bills the tokens
  (`completion_tokens_details.reasoning_tokens` is read into usage), but the next turn carries
  none of it: a long tool conversation thinks afresh each turn. The route file says so.
- The host's copy of `ChatDialect` and the module conformance cases gain the variant; a route
  file naming an unknown dialect still fails at load.
- Unverified against the live API (no key used): that `prompt_cache_key` and `store` are
  accepted on every bound model, and the exact `reasoning_effort` set each model takes beyond
  what its page lists.

## Alternatives considered

- **Replay `reasoning_content` on this dialect too**, so the shared replay check passes: the
  API defines no such field, and OpenAI returns none; sending an undefined field for a case that
  never happens is wrong twice.
- **Bind every GPT profile and let the API refuse**: a bound profile the route cannot serve
  with tools is dead data, as ADR-0197 said of the whole route.
- **Name the dialect `openai`**: dialects are named by what they encode, not by a vendor; the
  same encoding serves any Chat Completions gateway that defines no thinking fields.
- **One shared account file for both OpenAI routes**: a shipped `accounts/openai.toml` would
  change `routes/openai.toml`'s inline credential from ADR-0197; the environment variable
  already serves both, so this waits for a need.

## Evidence

- `cargo test -p p1-provider-openai-chat`: the `effort_level` tests pin the body (every
  effort's wire name, the absent `thinking` and `reasoning_content`, `max_completion_tokens`,
  `prompt_cache_key`, `store: false`) and the composition rules; the shared suite runs without
  the replay check on the dialect.
- `cargo test -p p1-provider-openai --test api_key`: the API-key body says `store: false`.
- `cargo test -p p1-host --test route_files --test route_validation --test models --test cli
  --test anthropic_route --test openai_route` and `cargo test -p p1-module-tests --test
  provider_conformance`: the shipped `openai-chat` route composes through the provider
  component, passes the suite, and every shipped chat dialect has a module case.
- OpenAI documentation as cited above, fetched 2026-10-11.
