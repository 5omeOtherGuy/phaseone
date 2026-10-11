---
adr: 197
title: API-key dialects and pay-as-you-go core routes; the default environment is default
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/847, https://github.com/5omeOtherGuy/phaseone/issues/840, https://github.com/5omeOtherGuy/phaseone/issues/766, docs/adr/0039-a-provider-is-composed-from-a-wire-adapter-a-route-and-a-model-profile.md, docs/adr/0093-route-settings-validation-is-host-owned-and-the-native-route-constructors-leave-p1-host.md, docs/adr/0139-accounts-are-separate-from-routes.md, docs/design/routes-and-profiles.md, https://platform.claude.com/docs/en/build-with-claude/context-windows, https://developers.openai.com/api/docs/models/all]
---
# ADR-0197: API-key dialects and pay-as-you-go core routes; the default environment is default

## Context

Owner, 2026-10-11 (epic #840, the boundary decision ADR-0200, pull request #856): "Currently, the phaseone repo mixes foundational work, like the ACP with my
own modules and how I would like phaseone to work for me (e.g. my system prompts, modes, etc.) We
need to figure out how to split those, so phaseone is only the bare bones product", and "even
the subscription connectores should not be part of the core product".

The core cannot run without a subscription today. The Anthropic Messages adapter knows the
dialects `claude-code-subscription`, `opencode-go` and `zai`; the first is the only one that
reaches Claude, and it always sends the Claude Code identity block and the OAuth betas. The
OpenAI Responses adapter knows only `codex-subscription` (`/codex/responses`, the ChatGPT
account id, `store: false`, the Codex client headers). No route file reaches a pay-as-you-go
API, `DEFAULT_ENV` is the owner's `claude` environment on the Claude Code subscription, and
`WorkflowSettings::shipped` names the owner's DeepSeek, GPT and Claude environments.

## Decision

1. **Dialect `api-key` on `anthropic-messages`**: the public Messages API with an API key. The
   key travels as `x-api-key` (`credential_header = "x-api-key"` is required, as for
   `opencode-go` and `zai`; the broker also sends it as a bearer, as on those routes);
   `anthropic-version` is sent; the prompt-cache markers of the subscription lane stay; no
   identity block, no OAuth betas, no `x-app` or browser-access header. `anthropic-beta`
   carries `interleaved-thinking-2025-05-14` only when the body has a manual thinking budget
   (the two `budget` profiles), and `context-1m-2025-08-07` only when the route sets
   `long_context`, alone when no other beta is sent. The Claude lowering (effort-level and
   budget profiles) applies unchanged.
2. **Dialect `api-key` on `openai-responses`**: `POST {endpoint}/v1/responses` (a base ending
   in `/v1` or `/v1/responses` works too) with the key as a bearer and p1's own `User-Agent`;
   no account-id header, no `originator`, no `OpenAI-Beta`, no session headers; `store` is not
   sent, so the API's default applies; an explicit output cap is sent as `max_output_tokens`;
   the cache key stays the body's `prompt_cache_key`. SSE only: the WebSocket transport speaks
   the Codex handshake, so `transport = "websocket"` on this dialect is refused at composition.
3. **Core routes** with an inline `[credential] method = "api-key"` (implicit accounts,
   ADR-0139 §6): `routes/anthropic.toml` (`https://api.anthropic.com`, `ANTHROPIC_API_KEY`,
   the six shipped Claude profiles) and `routes/openai.toml` (`https://api.openai.com`,
   `OPENAI_API_KEY`, the seven shipped GPT profiles, each listed by the OpenAI API as a
   current model served on `/v1/responses`). No `openai-chat` route ships: the chat adapter
   refuses every shipped GPT profile (they are `effort-level`, which no chat dialect encodes)
   and both chat dialects send `thinking` and `reasoning_content` fields the OpenAI Chat
   Completions API does not define, so such a route could bind no profile. It needs a chat
   dialect of its own first.
4. **The `default` environment**: route `anthropic`, profile `claude-sonnet-5`, the `claude`
   environment's tools and limits without owner paths (`AGENTS.md`/`CLAUDE.md`, project skill
   roots only), and a `prompt.md` joined from the generic fragments in the order of the `main`
   profile of `docs/design/prompts.md` (no `finish`, `apply_patch` or `manager-*` fragment).
5. **The default environment is `default`**, replaced by `settings.toml`
   `default_environment = "<name>"`. It is what a bare run loads and the environment a bare
   profile name in `--model` or `default_model` is looked for first. `default_model`, when also
   set, still decides a bare run; `--env` and `--model` still win over both.
6. **Workflow roles** ship on the core model: every role is `default/claude-sonnet-5` (reviewer
   and judge at `:high`) with no fallback chain and the same tool grants as before. The
   `claude-fable-5` cap stays: a user table that names Fable keeps the three-attempt limit.

## Consequences

- p1 runs with nothing but an Anthropic API key, and an OpenAI API key reaches the GPT models
  through a user environment that names the `openai` route. Both routes are paid per token.
- The owner keeps today's behaviour by adding this to `~/.config/p1/settings.toml`:

  ```toml
  default_environment = "claude"

  [workflows.roles.worker]
  model = "deepseek2/deepseek-v4.1-flash"
  fallback = ["gpt/gpt-6-sol", "claude/claude-opus-5-5"]
  tools = ["read", "grep", "edit", "shell"]

  [workflows.roles.reviewer]
  model = "claude/claude-opus-5-5:high"
  tools = ["read", "grep", "shell"]

  [workflows.roles.verifier]
  model = "deepseek2/deepseek-v4.1-flash"
  fallback = ["gpt/gpt-6-sol"]
  tools = ["read", "grep", "shell"]

  [workflows.roles.judge]
  model = "claude/claude-fable-5"
  tools = ["read", "grep"]
  ```

- A bare Claude profile name is now bound in `default` as well as `claude` and `claude2`: without
  `default_environment = "claude"`, `--model claude-opus-5` from no environment selects
  `default/claude-opus-5` (the API-key route), and from a third environment it is ambiguous, as
  it already was between `claude` and `claude2`.
- `--model` without `--env` now reads `settings.toml` for `default_environment`, so a malformed
  settings file fails such a run as it already failed a bare one.
- The API-key Responses route stores responses server-side for the API's retention period,
  which the Codex route never did; a route that must not store needs a dialect option later.
- `request_path` and `resolve_base_url` of `p1-provider-openai` take the account, because the
  path depends on it; the provider module's `http_target` follows.
- The public Responses API may require organization verification for reasoning summaries
  (`reasoning.summary = "auto"`, sent whenever an effort is set); untested here.

## Alternatives considered

- **A `none` dialect** (send only what the wire protocol requires, no account semantics): it
  would be either `api-key` under another name or unable to say where the key goes; the
  dialect is named by the account behaviour, as the existing ones are.
- **Keep Claude Code as the default**: the core would keep needing the owner's subscription,
  which is what epic #840 removes.
- **Ship `routes/openai-chat.toml` anyway**: a route that binds no profile, or binds profiles
  the adapter refuses at composition, is dead data.

## Evidence

- `cargo test -p p1-provider-anthropic --test api_key` and `cargo test -p p1-provider-openai
  --test api_key`: the lanes' bodies and headers, including the absent identity block, betas,
  `chatgpt-account-id`, `originator`, session headers and `store`, and the shared conformance
  suite over the Responses API-key route.
- `cargo test -p p1-host --test anthropic_route --test openai_route --test route_validation
  --test models`, `cargo test -p p1-host --lib routes`, `cargo test -p p1-module-tests --test
  provider_conformance`: the shipped routes compose through the provider components, the host's
  settings copies agree with the adapters, `default` assembles, and the default-environment
  precedence.
- A request to `https://api.anthropic.com/v1/messages` with a made-up `sk-ant-api03-…` key as
  bearer, as `x-api-key`, and as both, answered `invalid x-api-key` all three times
  (2026-10-11): the API reads the key the broker sends in either header as an API key.
- The Claude API documents a 1M-token default window for every bound Claude model, so the route
  sends no context beta; it lists `claude-sonnet-5`, `claude-opus-5`, `claude-opus-5-5`,
  `claude-fable-5`, `claude-opus-4-6` and `claude-sonnet-4-6` as available (fetched
  2026-10-11). The OpenAI model list shows all seven bound GPT ids as current, the model pages
  list `v1/responses` as supported, and `gpt-6-sol`'s page says Chat Completions "supports
  function calling only with `reasoning_effort` set to `none`".
