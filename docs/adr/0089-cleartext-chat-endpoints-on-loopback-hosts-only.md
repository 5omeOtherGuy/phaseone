---
adr: 89
title: Cleartext chat endpoints on loopback hosts only
status: proposed
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [PR #379 (S7.7.2, issue #314), XO decision D-XO-42 as cited in PR #379, crates/p1-provider-openai-chat/src/lib.rs, modules/p1-module-provider-openai-chat/src/settings.rs]
---
# ADR-0089: Cleartext chat endpoints on loopback hosts only

## Context

The `openai-chat` route's endpoint rule (`ChatRoute::validate` in
`crates/p1-provider-openai-chat/src/lib.rs`, reached by the native provider and, through the
shared `validate_composition`, by the `p1/provider-openai-chat` component's `configure`)
accepted `https://` endpoints only, while the broker already accepts a cleartext endpoint.
S7.7.2 (PR #379) runs an installed release offline against a provider that is a listener in
the test process on `127.0.0.1`, so the route it names is `http://127.0.0.1:<port>/...`. The
XO decision D-XO-42, cited in PR #379, adds one exception for that case; this ADR records it in
the repository's decision record, since it changes which hosts a route may send unencrypted
traffic to.

## Decision

A chat route's endpoint is `https://` for every host, except that a cleartext `http://`
endpoint is accepted when its host is exactly a loopback host (`127.0.0.1`, `[::1]` or
`localhost`), followed by nothing or a numeric port; every other `http://` endpoint is refused
with the existing message "chat endpoint requires HTTPS".

## Consequences

A test or an operator can point a route at a provider listening on the same machine without
TLS; such traffic does not leave the box. Lookalike hosts (`127.0.0.1.example.test`,
`localhost.evil.test`, `[::1].example.test`) and other addresses (`fe80::1`) keep requiring
`https`. The anthropic and openai adapters are unchanged. Other loopback forms (`127.0.0.2`,
`0:0:0:0:0:0:0:1`) are not accepted; widening the set needs a new decision.

## Alternatives considered

Serving the test provider over TLS with a test certificate, which would need a trust override
in the installed binary; keeping `https` only and not exercising an installed provider
component offline. Neither is recorded as chosen.

## Evidence

Unit tests `chat_takes_a_cleartext_endpoint_on_a_loopback_host_and_refuses_every_other`
(`crates/p1-provider-openai-chat/src/lib.rs`) and
`a_cleartext_endpoint_is_composed_on_a_loopback_host_alone`
(`modules/p1-module-provider-openai-chat/src/settings.rs`); the installed-release case
`an_installed_release_serves_a_tool_call_from_its_shipped_read_component`
(`crates/p1-module-tests/tests/installed_release.rs`).
