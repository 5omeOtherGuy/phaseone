---
adr: 93
title: Route settings validation is host-owned and the native route constructors leave p1-host
status: proposed
date: 2026-09-27
deciders: lead
supersedes: []
superseded_by: []
sources: [issue #393, PR #396 (description and Codex review of crates/p1-host/src/routes.rs), PR #396 lead comment 2026-09-27 13:30, docs/adr/0081-native-foundation-and-runtime-components.md, docs/adr/0086-provider-components-with-native-authenticated-transport.md, docs/design/routes-and-profiles.md, crates/p1-host/src/routes.rs, crates/p1-host/src/catalog/providers.rs, crates/p1-host/tests/native_routes/mod.rs, crates/p1-host/Cargo.toml]
---
# ADR-0093: Route settings validation is host-owned and the native route constructors leave p1-host

## Context

S7.10-R4 (issue #393, PR #396) takes the native HTTP provider crates `p1-provider-anthropic`,
`p1-provider-openai-chat` and `p1-provider-openai` out of `p1-host`'s normal dependency graph.
Two parts of `p1-host`'s public Rust interface depended on them:

1. `RouteFile::settings()` and the `AdapterSettings` variants typed a route's
   `[adapter_settings]` table with each adapter crate's own settings type, and
   `docs/design/routes-and-profiles.md` §1.2 said the adapter's struct deserializes the table.
2. `p1_host::catalog` exported the native route constructors `chat_route`, `messages_route` and
   `responses_route` (with their private `*_route_from` twins), which ADR-0086 names as the
   reference the provider components reproduce.

Codex's review of PR #396 found that this changes `p1-host`'s public interface and an ownership
boundary without a record; the lead asked for this ADR on the PR, and for ADR-0086's named
helpers to be superseded only through this ADR's text.

## Decision

1. **Route settings validation is host-owned.** `p1-host` checks `[adapter_settings]` when a
   route file loads with its own copies of the three adapters' settings types
   (`ChatAdapterSettings`, `MessagesAdapterSettings`, `ResponsesAdapterSettings` and their
   enums) in `crates/p1-host/src/routes.rs`: same type names, fields, variants, defaults and
   serde attributes, so a malformed route fails at the same stage with the same message. The
   `AdapterSettings` variants carry these host types. The provider component still parses its
   own settings type for a request (ADR-0086).
2. **The three native provider crates are test-only for `p1-host`.** They are
   `[dev-dependencies]` of `p1-host`; its normal graph keeps `p1-provider-http` as the only
   `p1-provider-*` crate.
3. **The native route constructors leave `p1-host`.** `chat_route`, `messages_route`,
   `responses_route` and the `*_route_from` helpers are removed from `p1_host::catalog`. The
   native composition lives in `crates/p1-host/tests/native_routes/mod.rs`, shared by path with
   `p1-module-tests` (with `lower_ceiling`), and is the reference ADR-0086 means where it
   names `chat_route_from` and `lower_ceiling` in `crates/p1-host/src/catalog/providers.rs`.

## Consequences

- The host's settings copies must follow any change to an adapter's settings type. Unit tests in
  `routes.rs` hold both sides equal on every shipped route and on every malformed case, with the
  adapter crates as the old side, so a drift fails the gate.
- Callers outside tests lose `p1_host::catalog::{chat_route, messages_route, responses_route}`;
  only tests used them.
- ADR-0086's text naming `chat_route_from` and `lower_ceiling` in
  `crates/p1-host/src/catalog/providers.rs` now reads as the test composition in
  `crates/p1-host/tests/native_routes/mod.rs`.

## Alternatives considered

- Keep the adapter crates as normal dependencies for settings validation only: rejected, it
  keeps them in the shipping graph S7.10 removes.
- Validate `[adapter_settings]` only in the provider component at request time: rejected, a
  malformed route file would no longer fail when it loads.

## Evidence

- `cargo tree --locked -p p1-host -e normal` lists no `p1-provider-anthropic`,
  `p1-provider-openai-chat` or `p1-provider-openai` (PR #396 description).
- `routes::tests::the_host_settings_agree_with_the_adapter_crates_on_every_shipped_route`,
  `..._on_every_case` and `a_malformed_route_fails_at_load_with_the_adapter_crates_error` in
  `crates/p1-host/src/routes.rs`.
