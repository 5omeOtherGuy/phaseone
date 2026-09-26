---
adr: 86
title: Provider components with native authenticated transport
status: proposed
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [issue #298, epic #206, DECISIONS.md D22, migration plan finding F2, docs/adr/0039-a-provider-is-composed-from-a-wire-adapter-a-route-and-a-model-profile.md, docs/adr/0046-an-exhausted-account-is-its-own-provider-error-kind-it-is-never-refreshed-or-retried.md, docs/adr/0061-a-route-may-be-self-contained-store-only-credentials-never-read-another-tool-s-login.md, docs/adr/0062-a-plan-refusal-is-its-own-provider-error-kind-it-is-never-refreshed-or-retried.md, docs/adr/0069-provider-reads-are-bounded-inside-the-connection-first-byte-120-s-stream-idle-300-s.md, docs/adr/0070-a-route-may-declare-no-credential-for-a-proxy-that-injects-it.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0078-connection-resources-and-component-replacement.md, docs/design/modules/wit.md, docs/design/modules/protocol.md, docs/design/modules/adapters.md, docs/design/routes-and-profiles.md, modules/wit/worlds.wit, modules/wit/transport.wit, crates/p1-host/src/routes.rs, crates/p1-host/src/catalog/providers.rs, crates/p1-provider-http/src/broker.rs, crates/p1-provider-http/src/drive.rs, crates/p1-provider-http/src/http.rs, crates/p1-provider-http/src/ws.rs, crates/p1-module-tests/tests/transport_authority.rs]
---
# ADR-0086: Provider components with native authenticated transport

## Context

ADR-0071 (DECISIONS.md D22) moves every provider into a WebAssembly module the host loads by
name; the agent core, the host and the transport stay native. ADR-0039 composes a provider from
three parts: a compiled wire adapter, a route file (`routes/<id>.toml`: how an account and an
endpoint are reached, with a credential reference) and a model profile (`profiles/<id>.toml`:
what the model is). Today all three meet inside `p1-host`: `routes.rs` parses and validates the
route file, `catalog/providers.rs` builds the adapter's route value from it, and the adapter
crate sends the request through `p1-provider-http`.

The frozen boundary (`wasm-boundary-v1`) already fixes the shape a component gets:

- World `p1:module/provider@1.0.0` (`modules/wit/worlds.wit`) exports `configure` with
  `provider-settings { origin-route, endpoint, model, wire-model, adapter-settings: json }`,
  plus `lower`, `classify` and `decoding.decoder`.
- `modules/wit/transport.wit` gives the component an `http-request` whose `path` starts with
  `/`, and a `credential-use` that names where a credential goes, never its value.
- `docs/design/modules/wit.md` (freeze item 9) states migration-plan finding F2: everything
  that sends, waits or retries is the native transport broker's; the component only translates.

The portable half of each adapter is on main (S4.1 #230, S4.3.1 #275), and so are the broker's
HTTP side (S4.2 #284, `crates/p1-provider-http/src/broker.rs`: `RouteAuthority`,
`LoweredHttpRequest`, `CredentialUse`, `broker_drive`, `check_lowered_headers`) and its
authority suite (S4.5 #279, `crates/p1-module-tests/tests/transport_authority.rs`). What was not
decided is which part of a route file the host keeps and which part reaches the component,
how an endpoint that is a full request URL fits a path-only request, and where the read bounds
of ADR-0069, still `proposed`, live once a component sits between the connection and the
adapter. This ADR is written before the first provider component (S4.3.2) lands, so that
component and the host's side of the split code against one text.

## Decision

**Placement (amends ADR-0039).** A provider is a wire-adapter component
(`p1/provider-anthropic`, `p1/provider-openai`, `p1/provider-openai-chat`, each of world
`p1:module/provider@1.0.0`) plus a route file plus a model profile. The adapter part of
ADR-0039 is no longer a compiled crate the host calls: it is a component the host configures.
The native transport broker — `p1-provider-http`'s native half with the `p1-auth` credential
source — sends every request.

**F2: the broker drives, the component translates.** Retry, backoff, the one credential refresh
after a 401 or 403 (with the rejected credential), the read bounds and cancellation are the
broker's. The component lowers the request (`lower`), classifies a non-2xx response
(`classify`) and decodes framed events (`decoding.decoder`), and nothing else. A component can
shape a failure's kind but can never trigger a retry, a refresh or a re-send. The broker's side
is S4.2 (#284): `broker_drive` hands a validated request to the existing `drive` loop. The
authority suite S4.5 (#279) holds it: a request that could leave the endpoint or carry a
credential header is refused before any credential is read, and the refresh happens once, with
the rejected credential.

**Route-file split.** The host keeps parsing and validating the whole route file exactly as
today (`crates/p1-host/src/routes.rs`, `load_route`), so every route-file error is still a load
error reported before any provider is built.

- The host reads, and uses for itself: `id`, `origin_route`, `adapter` (which component),
  `endpoint`, `[credential]` (a reference, never a value: ADR-0061 store-only, ADR-0070
  proxy-injected) and the `[models]` bindings. The endpoint and the credential source become
  the route's `RouteAuthority`, which no component can name or replace.
- The component reads, through `provider-settings.adapter-settings`, a JSON object: the route
  file's `[adapter_settings]` table unchanged (TOML to JSON), plus three reserved keys the host
  adds:
  - `model_profile` = `{ "stem": <profile file stem>, "toml": <profile file text> }`, which the
    component parses with `ModelProfile::from_toml(stem, toml)`;
  - `route_headers` = the route's `[headers]` table as an object of strings, empty when the
    table is absent;
  - `model_binding` = `{ "context_limit": <u64>?, "output_limit": <u32>? }`, where an absent key
    means unknown, never zero.
- The component removes the three reserved keys, then deserializes the rest as its adapter's
  settings type. Those types deny unknown fields, so no adapter settings field may ever be named
  `model_profile`, `route_headers` or `model_binding`.
- `origin-route`, `endpoint` (the route file's endpoint, unchanged), `model` (the profile id)
  and `wire-model` travel in their own `provider-settings` fields.
- The host builds the object with `RouteFile::component_adapter_settings` in
  `crates/p1-host/src/routes.rs`.
- The component reproduces the native route value exactly. For openai-chat, the static headers
  are the compiled `user-agent` followed by `route_headers` in name order, as `chat_route_from`
  builds them. `max_output_tokens` is the profile's ceiling lowered by `output_limit`, as
  `lower_ceiling` computes it (`crates/p1-host/src/catalog/providers.rs`).

**Endpoint and path.** The frozen `http-request.path` starts with `/`, and the broker refuses
any other path. Some routes use the full request URL as their endpoint: every openai-chat route
(`https://h/v1/chat/completions`), and an openai route whose endpoint already ends in
`/codex/responses`. Their portable lowering produces an empty path, so the broker gets a split:

- `RouteAuthority` gets the endpoint without its last path segment (`https://h/v1`);
- the component lowers that segment as the path (`/chat/completions`).

`provider-settings.endpoint` still carries the unchanged endpoint. The split rule lives in one
portable function that both the component and the conformance-over-components suite (S4.7)
call. Endpoint plus path is the same URL as before, so no request URL changes.

**Credential header position.** The broker attaches the credential ahead of the component's
headers, as the frozen transport text says. The native anthropic and openai-chat adapters put
it mid-list. Header order has no meaning for these endpoints. The conformance-over-components
suite (S4.7) therefore checks two things: the non-credential headers, in order, and whether each
credential header is present.

**Error parity (ADR-0046, ADR-0062).** `classify` returns the same closed `ProviderErrorKind`
that the native `ResponseParser::on_http_error` returns for the same response.

- `InsufficientBalance`, `NotEntitled` and `UsageLimitExhausted` are never refreshed or retried
  by the broker, whatever the component returns. The broker applies its own policy to the
  status class and the returned kind, exactly as `drive` does for a native adapter.
- A module failure maps as `docs/design/modules/protocol.md` fixes it: a trap or invalid output
  is `Protocol`, never retried; a deadline is `Transport`, within the broker's own retry policy;
  cancellation is `Cancelled`.
- No new kind is added.
- A settings object the component cannot parse makes `configure` return a `provider-error`, so
  the provider is never built and no request is sent. How the host reports that error before
  the first turn is decided with activation (S4.9).

**ADR-0069 reconciled (amends ADR-0069's placement, not its values).** ADR-0069's bounds stay in
the broker's frame loop, which sees every byte and every control frame:

- the first response bound (120 s);
- the stream idle bound (300 s);
- any received bytes or control frame (an SSE comment, a WebSocket ping or pong) resets
  transport idleness;
- the one waiting notice after 30 s without a response;
- no total timeout on an active stream.

A component sees no time, and it sees bytes only after the broker has framed them, so it can
neither extend nor shorten a bound. ADR-0069 becomes `accepted` alongside this decision.

**What this does not decide.** The WebSocket branch of the Responses route (ADR-0047, ADR-0078)
is S5's. Component activation and the module references in `environments/*.toml` and
`profiles/` are S4.9's, on `wasm-loader-v1`. Provider construction in
`crates/p1-host/src/catalog/providers.rs` does not change until S4.9 wires components.

## Consequences

- One route file still describes one route, for the native adapters and for the components. A
  shipped route file needs no edit. The unit tests in `crates/p1-host/src/routes.rs` show that,
  for every shipped route and every model it binds, the component's view parses into the same
  settings value `RouteFile::settings()` returns.
- Secrets stay on the host side. A component never receives a credential, a credential
  reference or a way to choose an endpoint: the route file's credential and endpoint become a
  `RouteAuthority` that no component can reach.
- The profile's text crosses the boundary once per `configure`, because the component cannot
  read files. That is a copy of non-secret data the host already reads.
- The three reserved keys are a contract between the host and every provider component. A
  future adapter settings field cannot take one of these names; the host's unit test fails if a
  settings type ever accepts one.
- Because the broker owns the retry and refresh policy, a faulty or hostile component cannot
  cause a retry storm or refresh an exhausted account. The worst it can do is report the wrong
  kind for one response. The conformance-over-components suite (S4.7) compares that kind with
  the native parser's.
- Endpoints that are full request URLs need the split rule, and the rule must stay identical
  in the component and in S4.7. It lives in one portable function for that reason.
- The credential header moves to the front of the header list. This is observable on the wire,
  meaningless to the endpoints, and accounted for in S4.7's header comparison.

## Alternatives considered

- **Hand the whole route file to the component.** Rejected: the component would see the
  credential reference and the endpoint and could build requests to any endpoint it named.
  RouteAuthority exists so that a component cannot choose an endpoint or a credential source.
- **Give the component read access to `profiles/`.** Rejected: it adds a filesystem capability
  to every provider only to read one small non-secret file the host already reads. Passing the
  stem and text keeps the provider world free of filesystem imports.
- **Add fields to `provider-settings` for the profile, headers and limits.** Rejected: the world
  is frozen (`wasm-boundary-v1`), and `adapter-settings` is already the JSON channel for the
  route data the adapter interprets.
- **Change the route files so that every endpoint is a base URL.** Rejected: it edits every
  shipped openai-chat route and the owner's own route files for a boundary detail. Splitting at
  the broker keeps every request URL and every file unchanged.
- **Let the component own retry or refresh for its vendor's quirks.** Rejected by F2: one
  shared budget and one refresh rule across all providers, and ADR-0046/ADR-0062 hold regardless
  of what a component returns.
- **Re-arm ADR-0069's bounds around the component's calls.** Rejected for the reason ADR-0069
  itself records: a timer outside the loop that consumes control frames calls a keep-alive peer
  idle.

## Evidence

- The host side of the split is `RouteFile::component_adapter_settings` in
  `crates/p1-host/src/routes.rs`. Its unit tests (`cargo test --locked -p p1-host routes`) are:
  - `every_shipped_route_and_binding_yields_the_component_settings_object`: every shipped
    route file and every bound profile; checks the reserved keys and parity with
    `RouteFile::settings()`;
  - `no_adapter_settings_type_accepts_a_reserved_key`;
  - `route_headers_and_known_limits_travel_and_unknown_limits_stay_absent`;
  - `a_route_without_adapter_settings_or_headers_still_carries_the_reserved_keys`.
- The broker's authority, refresh and account-diagnosis rules are in
  `crates/p1-provider-http/src/broker.rs` (S4.2, #284). The suite that holds them is
  `crates/p1-module-tests/tests/transport_authority.rs` (S4.5, #279), including
  `the_credential_header_is_first_exactly_once_and_component_headers_follow_unchanged`,
  `a_401_or_403_refreshes_once_with_the_rejected_credential` and
  `account_diagnoses_are_never_refreshed`.
- The read bounds are in `crates/p1-provider-http/src/http.rs` (`FIRST_BYTE_TIMEOUT`,
  `STREAM_IDLE_TIMEOUT`), `drive.rs` (the SSE loop and `WAITING_NOTE_AFTER`) and `ws.rs`
  (`read_bounded`). ADR-0069's Evidence lists the tests that hold them.
- The module error mapping is in `docs/design/modules/protocol.md` (freeze item 5), and the F2
  division is in `docs/design/modules/wit.md` (freeze item 9).
- Issue #298 is the S4.6 slice; its checks are `python3 scripts/adr.py check`, the `p1-host`
  route tests and clippy.
