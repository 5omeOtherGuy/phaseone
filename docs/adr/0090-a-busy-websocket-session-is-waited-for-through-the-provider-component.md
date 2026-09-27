---
adr: 90
title: A busy WebSocket session is waited for through the provider component
status: proposed
date: 2026-09-27
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/websocket.md, docs/adr/0078-connection-resources-and-component-replacement.md]
---
# ADR-0090: A busy WebSocket session is waited for through the provider component

## Context

S7.10-R5 (#394, PR #397) moves the OpenAI Responses WebSocket route from the native
`OpenAiCodexProvider` to the `p1/provider-openai` component and the native transport broker.
`docs/design/websocket.md` §4 says a request that arrives while the provider's one connection is
busy (concurrent `stream` calls) goes over SSE: "never a second socket, never a wait". ADR-0078
says the migration keeps the WebSocket behaviour.

Through the component this cannot hold. The frozen `websocket.connection-state` (modules/wit/) has
no fact for a busy session. Reporting the socket `open` would make the component send on a
connection another response is still reading. Reporting a `handshake` would drop that live
connection, because `WsLease::send` closes the connection it replaces. The component's only HTTP
answer is the fallback, and that turns WebSocket off for the instance for good. The PR's
implementer raised this as a question for the lead, and Codex's review of the PR asked for an ADR,
since the change reverses a documented behaviour.

## Decision

Through the provider component, a request that finds the session busy waits for the lease and
races its own cancellation. The lease is released at the previous response's terminal event. It
does not fall back to SSE.

## Consequences

- The component and the broker stay consistent: only one response ever reads a connection, and
  no live connection is dropped to serve another request.
- A caller that holds one stream without polling or dropping it, and then awaits a second
  `stream` on the same provider, waits until the first stream ends or the second request is
  cancelled. Before this change the second request went over SSE at once.
- `docs/design/websocket.md` §4 and the `ws_session.rs` module doc describe the wait. The parity
  suite holds it (`a_request_waits_while_another_response_holds_the_connection`).
- This can be reversed without changing the WIT, for example by lowering a busy request on a
  separate, throwaway instance that speaks HTTP.

## Alternatives considered

- Keep busy→SSE by lowering the waiting request on a throwaway component instance whose
  connection state forces HTTP. This costs a second instance per concurrent request, and the
  throwaway instance's fallback must not reach the session.
- Add a `busy` fact to `websocket.connection-state`. Refused, because the WIT is frozen.

## Evidence

PR #397: `crates/p1-provider-http/src/ws_session.rs` (`lease()`), `crates/p1-provider-http/src/ws_drive.rs`,
and `crates/p1-module-tests/tests/provider_websocket.rs`, which runs the native adapter and the
component on the same scripts.
