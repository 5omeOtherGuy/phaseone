# WebSocket transport for the Responses adapter

ADR-0047 (owner directive 2026-09-21, research #41). Scope: the `openai-responses` adapter only.
No other route has a documented WebSocket; they are untouched. `p1-core` and `p1-contracts` do
not change. Wire facts below are from the vendor's WebSocket-mode guide, `openai/codex@d992132`
and the read-only donor `iris-agent@62c8345` (`src/mimir/providers/openai_codex_responses.rs`);
each is marked [vendor], [upstream] or [donor].

## 1. Route setting

`routes/<id>.toml`, `[adapter_settings]`, adapter `openai-responses` only:

```toml
transport = "sse"        # default when absent; or "websocket"
```

Any other value, or the key on another adapter, is a route-file error (fail fast, as every other
unknown setting). `"websocket"` means: try WebSocket, fall back to SSE by the rules of §5.
**Owner decision 2026-09-21: WebSocket is the default wherever a route supports it** — the
shipped Codex route sets `transport = "websocket"` (the adapter's own default for a route file
without the key stays `sse`, so a new Responses route opts in explicitly).

The host composes the real connector; `catalog::route_provider` takes the connector as a
parameter next to the HTTP transport, so tests and live checks inject theirs (a test that
composes a shipped route must never open a real socket — AGENTS.md: no live network in tests).

## 2. The connector seam — `p1-provider-http::ws`

```rust
pub trait WsConnector: Send + Sync {
    fn connect<'a>(&'a self, request: WsHandshake)
        -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>>;
}
pub struct WsHandshake { pub url: String, pub headers: Vec<(String, String)> }
pub trait WsConnection: Send {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>>;
    /// The next TEXT payload. Binary frames are decoded as UTF-8; ping is answered and pong
    /// ignored inside the implementation; a close frame or end of stream is `Ok(None)`.
    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>>;
}
pub enum WsConnectError { Status { status: u16, body: Vec<u8> }, Failed(String) }
pub struct WsError(pub String);
```

- The real connector (`TungsteniteConnector`) is the ONLY code that names `tokio-tungstenite`
  (`default-features = false`, features `connect`, `rustls-tls-webpki-roots`). A rejected upgrade
  surfaces its HTTP status and body as `WsConnectError::Status`.
- Behind the existing `testing` feature: `ScriptedWsConnector` — scripted connections (refuse
  with a status, or accept and then yield scripted text frames / errors / a close), recording
  every handshake (url, header NAMES and values) and every sent text frame. No network.
- No `Debug`, `Display` or error message ever contains a header value.

## 3. Handshake and framing

- URL: the HTTPS URL the adapter already resolves, scheme swapped `https→wss`, `http→ws`
  [donor, upstream]. For the shipped route: `wss://chatgpt.com/backend-api/codex/responses`.
- Headers, in this order: `Authorization`, `chatgpt-account-id` (codex account only),
  `originator`, `User-Agent` (p1's existing values), `OpenAI-Beta: responses_websockets=2026-02-06`
  [donor, upstream], and — when the request has a cache key — `session-id: <key>` and
  `x-client-request-id: p1-<key>`. No `Content-Type`, no `Accept`.
- A request is ONE text frame: the JSON body the SSE path would send, minus `stream` and
  `background`, plus `"type": "response.create"` at top level [donor; vendor: those fields are
  not used].
- Each received text is ONE JSON event with the vocabulary the SSE parser already dispatches on.
  It is fed to the EXISTING `ResponseParser` as an event with that data and no event name. No
  second parser. The response ends at the parser's terminal event; the connection stays open.

## 4. Connection lifetime (per provider instance)

- One connection, behind an async mutex, reused across turns. If it is busy when a request
  arrives (concurrent `stream` calls), that request uses SSE — never a second socket, never a wait.
- Reuse only while `age < 55 min` and `idle < 5 min` [donor; vendor: connections last 60 min];
  otherwise drop it and connect anew. Time comes from an injected clock, as elsewhere in the crate.
- Connect and send are bounded by 10 s each; reads by the adapter's existing idle timeout.
  Every wait races the request's `CancellationToken`.
- A cancelled or failed response DROPS the connection (a half-read socket is never reused) and
  clears the continuation of §6. Dropping the returned stream counts as cancellation.
- A connection returns to the slot only after a response completed cleanly.

## 5. Failure policy (the WebSocket form of `drive()`'s rules)

Before any model-visible output of this request:
| Failure | Action |
|---|---|
| Upgrade refused 401/403 | ONE forced credential refresh, reconnect once; refused again → `Authentication` |
| Upgrade refused 429 | `RateLimited` (no fallback: SSE would hit the same limit) |
| Upgrade refused, any other status (the endpoint says no) | fall back to SSE at once |
| Error event `previous_response_not_found` | reconnect, send FULL input, once |
| Error event `websocket_connection_limit_reached` | reconnect, send FULL input, once |
| A reused socket closes before its first frame | reconnect, send FULL input, once |
| Any other error event | what the existing parser makes of it (same kinds as SSE) |
| Connect error or timeout; read/send error or close before visible output | reconnect with the FULL body, with the adapter's retry policy's backoff, up to its `max_retries` (default 3) — then fall back to SSE |

"Fall back to SSE" = run today's `drive()` path for THIS request and turn WebSocket off for this
provider instance (until the process ends). Reconnects per request: one for each of the three
"once" rows, and up to `max_retries` for the transient row [donor `WsRecoveryState`: retries
within the budget, then `FallbackSse` + `disable_ws_for_session`; after visible output `Fatal`]. After
model-visible output, every failure is an ordinary `Transport` failure of that response — no
retry inside the adapter, no fallback (the host's turn-level retry, completion.md §3b, applies).
`InsufficientBalance` (ADR-0046) is emitted wherever the SSE path would emit it.

## 6. Continuation (stage 2)

The adapter remembers, per live connection: the last request body it sent in FULL form, the id
of the response it got, and the output items of that response as they would appear in the next
request's `input`.
A request is sent as a continuation — `previous_response_id` = that id, `input` = only the new
items — exactly when ALL hold [donor `continuation_delta`/`same_continuation_shape`, upstream
`get_incremental_items`]:
1. the connection is the one that produced that response, and it completed cleanly;
2. every top-level field of the new body except `input` equals the remembered one;
3. the new `input` starts with the remembered `input` followed by the remembered output items,
   compared as JSON values, and has at least one more item.
Otherwise the FULL body is sent; never an error. After a context replacement rule 3 fails by
construction, so the full body goes out. The memory is cleared whenever the connection is dropped.
The frozen assertion that the SSE path never sends `previous_response_id` stays true.

## 7. What does not change

`Origin`, the journal and replay identity (the transport is not part of a response's origin;
ADR-0033); `prompt_cache_key`; the usage a response reports; every SSE test. Resume in a new
process starts with a new connection and a full body.

## 8. Stages and verification

- **Stage A — seam** (`p1-provider-http`): §2 with the scripted peer and unit tests; the real
  connector compiles and is exercised by nothing but a loopback test against a local
  `tokio-tungstenite` server (no external network).
- **Stage B — framing** (`p1-provider-openai`, host route parsing): §1, §3, §4, §5 with FULL
  bodies only. Must-pass, all offline with the scripted peer: same `StreamEvent` sequence as the
  SSE fixture for the same events; handshake URL and header names; frame shape; every row of §5;
  busy connection → SSE; cancellation drops the socket; `transport` absent → byte-identical SSE.
- **Stage C — continuation**: §6, with tests for each of the three rules failing and for the
  full-body recovery rows of §5.
- **Live probe (lead only) — RUN 2026-09-21, see `docs/research/41-websocket.md`.** The
  subscription backend accepts the upgrade and the continuation. The criterion first written
  here (a lower `input_uncached` on turn ≥ 2) was the wrong metric: the server counts the
  remembered context as input, so reported usage does not change; on a small task neither cache
  share nor wall time differed between the arms. The shipped route therefore stays `sse`.
  A comparison on long sessions (upload size) is open and needs the transport to be visible to
  the operator first. No latency claim is made.
