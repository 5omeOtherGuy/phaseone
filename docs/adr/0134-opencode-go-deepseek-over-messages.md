---
adr: 134
title: OpenCode Go DeepSeek over Messages
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: [crates/p1-provider-anthropic/src/request.rs, crates/p1-host/src/routes.rs, modules/wit/transport.wit, routes/opencode-go-messages.toml, environments/deepseek-messages/environment.toml]
---
# ADR-0134: OpenCode Go DeepSeek over Messages

## Context
Issue #622 row R5 was selected by the owner for implementation in #623. The installed
DeepSeek harness uses the Anthropic Messages wire rather than Chat Completions. The
research probes M5–M7 found OpenCode Go serves `/zen/go/v1/messages`, requires
`x-opencode-session`, accepts thinking/signature and tool-result replay, and reports
native Messages cache fields. The existing Messages adapter assumed Claude OAuth,
mandatory Claude Code identity, adaptive/manual thinking and explicit cache markers.

## Decision
Add `account = "opencode-go"` to the existing Messages adapter, not another provider
crate. On this account an enabled-thinking profile lowers to enabled/disabled thinking,
with low/high/max `output_config.effort` only when enabled. An explicit output cap wins;
otherwise the profile cap wins, then the installed dsh default of 256,000. The endpoint
is the API root `https://opencode.ai/zen/go`; the adapter appends `/v1/messages`.

This account sends no Claude identity, OAuth/interleaved/long-context beta, or
`cache_control`, `tool_choice` or `strict` fields. Long-context beta configuration is
refused. The existing cache-key/session mechanism supplies `x-opencode-session`;
direct callers without a key use the configured origin route as their session key.
The host continues generating distinct keys for assembled environments/agents.

Extend credential-control's placement vocabulary with `bearer-and-api-key`: the native
adapter and host broker attach the same API key as Bearer and x-api-key, matching the
successful Go probe. Provider components still receive no credential value, and a
proxy-injected route still sends neither credential header. Claude's existing bearer
placement and request bytes remain unchanged.

Four new routes have distinct Messages replay origins and reuse the original Go
account store entries and environment variables. Optional top-level `credential_route`
names the store identity, not the replay identity. It is accepted only for store-only
API-key routes pointing to a shipped API-key route on the same endpoint origin. Reads,
inspection, login, trust and logout use that store identity; login/logout consequently
affect the shared account, and their output names the backing store route. Existing
endpoint-origin approval and locked-store checks remain in place.

The new `deepseek-messages` environment has the same options, tools and context settings
as `deepseek`. Its prompt is a relative symlink to the existing prompt, not a copied or
edited identity. No existing route or environment is switched. Thinking text and
signatures are replayed byte-exact on every later same-origin request, including
non-tool turns; foreign-origin reasoning still drops. The parser retains a signature
present at block start as well as subsequent signature deltas. Usage keeps uncached
input, cache read, cache creation and output separate, never adding cache to input twice.

## Consequences
The two wires can be measured without changing existing environments or account keys.
The module set must be rebuilt with the extended credential placement vocabulary;
old components continue emitting bearer, but the new Go component needs the matching
host broker. Sharing a store identity intentionally shares key rotation and logout.
The new environment tracks the current DeepSeek prompt, while its TOML configuration
is checked for equality except for the selected route. Provider/gateway limits and
cache performance remain route facts, not conclusions from this implementation.

## Alternatives considered
Switching existing Go routes: deferred to owner selection after measurement. Copying
credentials to new entries: rejected; rotation would leave duplicate stale keys.
Using Claude's existing account mode: rejected; it sends identity/betas and the wrong
thinking policy. Giving a module credential access or static secret headers: rejected;
placement remains a broker-only operation. Adding a free-form auth/header dialect or
a new provider crate: unnecessary for this one measured account behavior.

## Evidence
Spec: https://github.com/5omeOtherGuy/phaseone/issues/623; research:
https://github.com/5omeOtherGuy/phaseone/issues/622. Local reports in
`~/.agents/xo/dispatch/p1-lead-20261004/deepseek/`: report-dsh §1 and report-api M5–M7.
Installed `@deepseek-ai/dsh@0.2.0-rc.2`, adapter serializer lines 1694–1717 and
reasoning replay lines 1539–1558. Hand-authored stub tests cover the exact request,
non-ASCII thinking/signature replay, tool results, asymmetric cache counts, the real
guest/broker composition, shared credential identities and origin/kind refusals.
The pull request records executed tests and the bounded live smoke result.
