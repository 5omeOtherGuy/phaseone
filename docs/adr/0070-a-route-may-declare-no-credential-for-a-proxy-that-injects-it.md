---
adr: 70
title: A route may declare no credential for a proxy that injects it
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/credentials.md, docs/design/routes-and-profiles.md, docs/design/usage.md, crates/p1-auth/src/spec.rs, crates/p1-auth/src/resolve.rs, crates/p1-provider-http/src/credential.rs, crates/p1-provider-http/src/drive.rs, crates/p1-provider-openai-chat/src/lib.rs, crates/p1-provider-anthropic/src/request.rs, crates/p1-provider-openai/src/request.rs, crates/p1-provider-openai/src/websocket.rs, crates/p1-host/src/login.rs, crates/p1-usage/src/probe.rs]
---
# ADR-0070: A route may declare no credential for a proxy that injects it

## Context

Request from brain1 (2026-09-25), issue #134: in a Claude Code cloud session an EGRESS PROXY adds
the provider key per host after the request leaves the VM, so the session never sees it. p1 had no
way to send a request without its own credential:

- `CredentialKind` was `api-key` / `claude-code-oauth` / `codex-oauth`; every kind ended in a
  source p1 reads, and the driver treated every 401/403 as a possibly-rejected key that might be
  refreshed;
- `openai-chat` always sent `Authorization: Bearer <credential>`, `anthropic-messages` likewise,
  and `openai-responses` additionally sent the ChatGPT account id.

A stored placeholder key only works if the proxy overrides an existing `Authorization` header, and
the request says it may not. The alternative — inferring "send nothing" from an absent credential —
was explicitly rejected: a missing key on an `api-key` route must keep failing exactly as today,
and the route file must SAY that it sends no credential. This changes the route-file interface, so
it needs a decision.

## Decision

1. **A new `[credential] kind = "none"`.** It is written explicitly, never inferred. It has NO
   source: no environment variable, no store entry, no other tool's login is read or constructed,
   and there is nothing to refresh. Because it names no source, `env`, a nonempty `borrow` and
   `store_only = true` beside it are load errors.
2. **The signal is one defaulted trait method.** `CredentialSource::proxy_injected()` returns
   `false` for every source that resolves a credential of its own and `true` for this kind. Every
   adapter that would send a credential header sends none when it is true — `openai-chat`,
   `anthropic-messages` and `openai-responses`, on the SSE path and the WebSocket handshake — and
   the placeholder `access()` hands out has an EMPTY bearer that no adapter may send.
3. **Neither transport ever refreshes such a route.** A 401/403 that classifies as `Authentication`
   (not `InsufficientBalance`/`NotEntitled`, which keep their own diagnosis) finishes immediately as
   an Authentication failure naming the missing proxy credential and the status — never a key. That
   holds for the SSE driver AND for a WebSocket upgrade refused 401/403, which never enters its
   refresh phase; the two share one message
   (`p1_provider_http::proxy_refusal_message`), so they cannot drift apart. `refresh(rejected)`
   refuses the same way for any direct caller.
4. **It is visible.** `p1 login --list` prints the kind as `none (proxy-injected)` and the source
   line as `none (proxy-injected) — the egress proxy injects the credential`; `p1 login`/`logout`
   on such a route are usage errors (p1 stores nothing for it), and `p1 usage` reports it
   `Unsupported` rather than probing with an empty bearer.

## Consequences

- A route can be reached through a proxy that injects the credential, with no placeholder secret
  and no header for the proxy to override.
- The change to the pinned crates is additive: `p1-provider-http` gains one defaulted trait method
  and one driver branch (`proxy_injected()` default `false` keeps every existing source and route
  byte-identical), and one new free function `proxy_refusal_message`, which the Responses WebSocket
  path shares so the SSE and handshake messages cannot drift. `p1-provider-openai-chat` gains
  nothing public. `p1-auth` is not pinned and gains the `CredentialKind::None` variant,
  `CredentialKind::label`, and `CredentialPolicy::ProxyInjected`.
- **`CredentialKind` is public and NOT `#[non_exhaustive]`, so `None` is a new variant of an
  exhaustive enum: a downstream consumer that matches `CredentialKind` exhaustively (the brain
  research harness consumes p1 crates as pinned git dependencies, issue #134) fails to compile
  until it adds an arm.** That is accepted rather than papered over: marking the enum
  `#[non_exhaustive]` would be a second source-compat change, and a downstream `_ =>` arm that
  silently treats a `none` route as one that holds a key is exactly the bug this ADR prevents. The
  blast radius is bounded because p1-auth is not pinned (unlike `p1-provider-openai-chat` and
  `p1-provider-http`), and every in-workspace match site is updated here.
- A `none` route cannot be probed for usage by p1, and `p1 login` cannot store anything for it.
- No shipped route uses the kind yet: it is interface for the brain research harness and the cloud
  sessions, which compose their own route files.
- An empty bearer is still refused as a credential everywhere else: the placeholder is only
  reachable through a `proxy_injected()` source, and each adapter's test pins that it sends no
  header.

## Alternatives considered

- **Infer "no credential" from a missing key.** Rejected by the issue: the route file names it
  explicitly, and a missing key on an `api-key` route must keep failing as today.
- **A placeholder key plus `kind = "api-key"`.** Works only if the proxy overrides an existing
  `Authorization` header; the request says it may not, and it would put a fake secret on the wire.
- **A policy field (`send_no_credential = true`) instead of a kind.** A kind is what the credential
  table already models, and the policy field (`store_only`) exists to narrow a chain this kind does
  not have.
- **An adapter-level opt-in per route (`[adapter_settings] no_auth = true`).** Credentials belong to
  the route's `[credential]` table (ADR-0040), not to a wire adapter's settings; three adapters
  would each re-implement the same statement.
- **Treat the 401 as a refresh failure.** Rejected: there is nothing to refresh, and a refresh
  error would mask the proxy's refusal with p1's own vocabulary.

## Evidence

- `crates/p1-auth/tests/none.rs`: the table, the refusal of contradictory fields, the
  `ProxyInjected` report line, the empty placeholder, and the negative proof that a 0644 store
  entry and a directory-where-a-login-belongs are never read; the contrast case shows the same home
  answering an `api-key` route from the store.
- `crates/p1-auth/tests/credential_table.rs`: `none` parses by its route-file spelling and the
  unknown-kind error lists it.
- `crates/p1-provider-http/src/drive.rs` (`a_proxy_injected_route_is_never_refreshed_and_a_401_names_the_proxy_credential`):
  one request, no refresh call, an Authentication failure naming the proxy credential.
- `crates/p1-provider-{openai-chat,anthropic,openai}/tests/credential_none.rs`: each adapter's
  request carries no credential header when the source is proxy-injected, and still carries it
  otherwise. The Responses file also covers the WebSocket half: a proxy-injected handshake's header
  list is §3's minus `Authorization` and `chatgpt-account-id`, a resolving credential still sends
  both, a `refuse(401)` upgrade ends in ONE handshake and an Authentication failure naming the proxy
  credential with no refresh call, and the same refusal on a resolving route still refreshes once.
- `crates/p1-host/tests/none_credential.rs`: the SHIPPED route files with their `[credential]`
  table replaced, loaded and composed through the host's own loader and catalog factory, reach the
  transport with no credential header for all three adapters; the same route with an `api-key`
  table still sends `Bearer <key>`; an unknown kind is still a route-file error.
- `crates/p1-host/tests/login.rs`
  (`a_none_route_is_listed_as_proxy_injected_and_cannot_be_logged_in`): `p1 login --list` shows
  `none (proxy-injected)`, and `p1 login`/`logout` are usage errors that read no key.
