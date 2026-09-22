---
adr: 47
title: The Codex route may speak WebSocket: an adapter-local transport with SSE as the fallback
status: accepted
date: 2026-09-21
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/design/websocket.md, docs/design/routes.md]
---
# ADR-0047: The Codex route may speak WebSocket: an adapter-local transport with SSE as the fallback

## Context

`docs/design/routes.md` §B and the Responses adapter's module comment record that the donor's
WebSocket transport is deliberately NOT taken. The owner reversed that on 2026-09-21: "I want
websocket support wherever possible (e.g. codex models)". Research #41 established where it is
possible today: exactly one shipped route, the ChatGPT/Codex subscription. The vendor documents a
WebSocket mode for the Responses API in which a turn continues with `previous_response_id` and
only the NEW input items, the prior state living in a connection-local cache. On this route that
is the only way to stop resending the whole conversation every turn, because the subscription
account sends `store: false` and therefore cannot use `previous_response_id` over HTTPS. The
upstream Codex CLI and the donor agree on the wire: same path with `wss://`, one JSON text frame
per event, the event vocabulary p1's SSE parser already understands. For the other four routes no
vendor documents a WebSocket; they stay on SSE until one does.

## Decision

A Responses route may set `transport = "websocket"` in `[adapter_settings]`; the default stays
`"sse"`. The WebSocket path is adapter-local: `p1-provider-http` gains a small connector seam
(`WsConnector`/`WsConnection`, real implementation on `tokio-tungstenite`, a scripted peer for
tests) and `p1-provider-openai` gains a module that frames requests, feeds the EXISTING parser and
owns the connection's lifetime; it does not go through `drive()`. `p1-core` and `p1-contracts` do
not change. Any failure before model-visible output falls back to today's SSE request and turns
WebSocket off for that provider instance; a failure after visible output is an ordinary
`Transport` failure. It lands in two stages: framing with full input (behaviour-identical), then
continuation. The shipped route file switches to `websocket` only after a lead-run live probe.
`docs/design/websocket.md` is the specification.

## Consequences

- One new third-party dependency, `tokio-tungstenite` (rustls, no default features), confined to
  `p1-provider-http`. The owner's directive is the approval the dependency rule asks for.
- The status-keyed policy of `drive()` (one credential refresh on 401/403, rate limiting,
  `InsufficientBalance`) has to be re-expressed for a handshake and for error frames; two code
  paths now carry that policy and must be kept in step by tests.
- A saving exists only from the second turn on one live connection: none on the first turn, after
  a reconnect, after a context replacement, or on resume in a new process. Nothing is claimed
  about latency or cost until the live probe and dogfood records say so.
- Journal, `Origin` and replay identity are untouched: the transport is not part of a response's
  origin, so a session may move between SSE and WebSocket freely (ADR-0033 holds).
- Rollback is one line in the route file.

## Alternatives considered

- Carry WebSocket inside the `Transport::post` seam by re-framing frames as SSE: no new code path
  through the adapter, but it cannot carry `previous_response_id` or a connection's lifetime — all
  of the cost, none of the saving.
- Make it the default at once: refused until a live connect against the subscription backend has
  succeeded; upstream code points at it, no vendor sentence does.
- WebSocket for the other routes: no primary source documents one; reopen per route when one does.

## Evidence

`../phaseone-briefs/research/41/memo.md`: vendor WebSocket-mode guide (re-fetched 2026-09-21),
`openai/codex@d992132` (`codex-api/src/provider.rs:89-100`, `common.rs:367-371`,
`model-provider-info/src/lib.rs:77,547,647`), donor `iris-agent@62c8345`
`src/mimir/providers/openai_codex_responses.rs`. No measurement exists yet.
