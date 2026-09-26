---
adr: 69
title: Provider reads are bounded inside the connection: first byte 120 s, stream idle 300 s
status: accepted
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [crates/p1-provider-http/src/http.rs, crates/p1-provider-http/src/lib.rs, crates/p1-provider-http/src/drive.rs, crates/p1-provider-http/src/ws.rs, crates/p1-provider-http/src/testing.rs, crates/p1-provider-openai/src/websocket.rs, crates/p1-provider-openai/tests/websocket.rs, docs/design/providers.md, docs/design/websocket.md]
---
# ADR-0069: Provider reads are bounded inside the connection: first byte 120 s, stream idle 300 s

## Context

Issue #164's second problem: p1 bounded only a connect (30 s), so a provider that accepted the
request and then never answered hung the agent silently — the operator saw runs killed at 300 s
and 240 s with no event, no error, and no way to tell "still working" from "gone". Two waits were
unbounded on BOTH provider paths: the wait for the response headers (HTTP) or the first frame
(WebSocket) after the request went out, and the wait between two events of an already-open stream.

The straightforward place to add a deadline is the adapter, around the read it performs. That is
where the WebSocket adapter first put it, and an independent review of PR #168 found why it is the
wrong place: the adapter awaits one "give me the next text" call, but the CONNECTION's message loop
consumes control frames inside that call — it answers a ping and loops without returning — so a
timer around the call cannot see a peer that keeps its socket alive with WebSocket pings, and calls
a live stream idle. A keep-alive peer would be failed as silent exactly when it was trying hardest
to stay alive. The bound has to be re-armed where every frame is visible: the message loop.

The lead's decision for issue #164, still in force: two bounds, both provider paths, the EXISTING
`ProviderErrorKind::Transport` (p1-contracts is pinned by two downstream tools — add no enum
variant, change no public type), the message names the bound, the values are constants with no new
config surface, and the tests use fake time and the crate's existing test transport.

## Decision

1. **Two bounds live in `p1-provider-http`, one definition each.** `FIRST_BYTE_TIMEOUT` = 120 s is
   the wait for the response headers (HTTP) or the first frame (a WebSocket after a send);
   `STREAM_IDLE_TIMEOUT` = 300 s is the wait between two chunks or frames of an OPEN stream. Any
   received bytes or frame — an SSE comment, a WebSocket ping or pong — reset the idle clock, so a
   keep-alive peer is never called idle. They are `pub const`s in the crate that owns the shared
   request policy, not a route key and not a config knob: no route has a reason to tune them.
2. **The WebSocket deadline lives in the connection's message loop, not in the adapter.**
   `ws.rs::read_bounded` picks `WsBound::FirstFrame` until the first message arrives and
   `WsBound::Idle` after it, and re-arms `tokio::time::timeout` around the next raw message on
   EVERY iteration — text, binary, ping, pong — answering a ping in the loop. The adapter only
   classifies what the connection reports. The pong write that answers a ping is bounded too (10 s,
   like a caller's own send), so a peer that stops reading can no longer block the read forever.
3. **An expiry is the existing `ProviderErrorKind::Transport`, with a message that names the
   bound.** `first_byte_timeout_message()`/`stream_idle_timeout_message()` are the ONE wording for
   both transports ("no response within 120 s" / "stream idle for 300 s"). p1-contracts gains no
   variant and no public type; the SSE driver and the WebSocket adapter produce the same kind and
   the same wording.
4. **The public seam grows additively.** `WsNext` (`Text`/`Closed`/`Timeout(bound)`) and `WsBound`
   (`FirstFrame`/`Idle`, with `limit()` and `message()`) are new public enums, and
   `WsConnection::next_bounded` is a new PROVIDED method, so an existing implementor still
   compiles. The default forwards `next_text` with NO bound — all a simple test double needs — and
   its documentation states the obligation that follows: a real connection, or ANY decorator that
   wraps one, MUST override it, or the read is unbounded and an expiry looks like a close.
   `next_text` folds a bound expiry into `Ok(None)`, so a legacy caller cannot tell it from a
   close; a caller that must name the bound reads `next_bounded`.
5. **The SSE first-byte wait says so, once, through an existing event type.** After
   `WAITING_NOTE_AFTER` = 30 s without a response the driver queues ONE `StreamEvent::Notice`
   ("waiting for the provider (30 s)", ADR-0048) and keeps the same request in flight; the note
   resets no clock and is not output, so no retry or terminal rule changes.

## Consequences

- A provider that never answers now ends as a named `Transport` failure instead of hanging. A
  first-byte expiry on a FRESH connection happens before any output, so it takes §5's transient
  row (a first-frame expiry on a REUSED WebSocket takes the once row and reconnects, like a reused
  socket that closes before its first frame; websocket.rs `on_read_timeout`): with the default
  `RetryPolicy` (1 + `max_retries` = 4 attempts, 2/4/8 s backoff) the operator sees the named error
  after at most 4 × 120 s + 14 s ≈ 8 min 14 s of post-connect silence (≈ 10 min 14 s if each
  attempt also burns the 30 s connect timeout). An idle expiry AFTER model-visible output is
  terminal — no retry, no fallback — and shows about 300 s after the last byte.
- Because the bound is per MESSAGE, the idle clock cannot be starved by control traffic: a peer
  that pings every 200 s for 20 minutes is alive for all 20 minutes. The tests drive the production
  loop with the crate's scripted channel, so this is the shipped clock that is proved, not a copy.
- The same bound now covers the WebSocket slot's reused connections: a first-frame expiry on a
  socket that was already open takes §5's "reused socket" once-row (reconnect once, no retry byte,
  no SSE fallback) rather than the transient row.
- The public surface grew: `WsNext`, `WsBound`, `WsConnection::next_bounded`, and the
  `testing::ScriptedFrame` control/pause variants. All are additive; the one hazard is the
  unbounded default, which the doc comment now names explicitly. The workspace's own p1-live
  decorator was updated to forward `next_bounded`, so the live check keeps the bound.
- HTTP gains no total timeout: a long, ACTIVE stream is still allowed to run for hours. Only
  silence is bounded, which is why the idle bound is per event and not per request.
- The `StreamEvent::Notice` for the wait is display-only: it carries a constant string, never a
  value, and adds no history, journal or model-visible content beyond what ADR-0048 already scopes.

## Alternatives considered

- **Bound the read in the adapter (per-call `tokio::time::timeout` around `next_text`).** Rejected:
  the connection's loop consumes pings inside that call, so the adapter's timer cannot see control
  frames and calls a keep-alive peer idle — the round-1 review's major defect on PR #168. The bound
  was moved into the connection's message loop instead.
- **Add a `ProviderErrorKind::Timeout` (or any new p1-contracts type).** Rejected by the lead's
  decision: p1-contracts is pinned by two downstream tools, and `Transport` already carries a
  pre-output/transient vs post-output/terminal distinction in both adapters. The message names the
  bound, which is what an operator needs.
- **Make the bounds configuration (a route key, a settings field, an environment variable).**
  Rejected: no route has a reason to tune them, and a tunable bound invites an operator to disable
  the protection the incident asked for. Constants keep the policy identical across routes.
- **Bound only the HTTP first byte (leave the open stream unbounded).** Rejected: the observed
  hangs were on streams that had already opened and then gone silent, so the idle wait is the one
  that ends the incident. The idle value (300 s) is deliberately larger than the first-byte value
  because a reasoning model may think for minutes before its next delta — but its stream still
  carries reasoning deltas or keep-alive pings well inside 300 s.
- **Bound the pong write by cancellation only (leave `channel.pong` awaitable forever).** Rejected:
  a peer that stops reading its socket while the write buffer fills would block the read loop
  exactly as the unbounded provider wait this ADR removes; the write is bounded like a caller's send.

## Evidence

- `crates/p1-provider-http/src/http.rs:113` (`FIRST_BYTE_TIMEOUT`), `:122`
  (`STREAM_IDLE_TIMEOUT`), `:126`/`:131` (the one wording per bound).
- `crates/p1-provider-http/src/drive.rs` — the SSE first-byte bound and its expiry
  (`first_byte_timeout`, :303; the deadline is armed per attempt), the idle bound on the body read
  (`stream_idle_timeout_message()`, :401), and the once-per-wait note (`WAITING_NOTE_AFTER`, :527;
  `await_post`, :249). Tests: `a_request_that_never_answers_fails_at_the_first_byte_bound`,
  `a_first_byte_timeout_retries_within_the_budget_then_fails_transport`,
  `a_stream_that_stalls_after_an_event_fails_at_the_idle_bound`,
  `keep_alive_pings_reset_the_idle_bound`,
  `the_first_byte_wait_tells_the_operator_once_after_thirty_seconds`,
  `a_response_that_arrived_inside_the_bound_wins_when_the_poll_resumes_late`.
- `crates/p1-provider-http/src/ws.rs` — `next_bounded` (:97, the provided, documented default),
  `WsBound` (:109), `WsNext` (:137), `read_bounded` (:288, re-arms per message),
  `PONG_WRITE_TIMEOUT` (:43). Tests: `a_control_ping_resets_the_idle_clock`,
  `a_pong_only_keep_alive_resets_the_idle_clock`, `silence_past_the_idle_bound_is_a_timeout`,
  `no_first_frame_past_the_first_byte_bound_is_a_timeout`,
  `a_pong_write_that_stalls_fails_the_read_instead_of_hanging` — all driven through the crate's
  real `read_bounded`.
- `crates/p1-provider-openai/src/websocket.rs` — `read` (:702) classifies the connection's
  `WsNext`; `on_read_timeout` (:786) names the bound, and gives a reused socket's first-frame
  expiry §5's once-row. Tests (`crates/p1-provider-openai/tests/websocket.rs`):
  `a_read_that_never_answers_is_bounded_at_the_first_frame_and_falls_back`,
  `a_stream_that_stalls_after_output_fails_at_the_idle_bound`,
  `keep_alive_pings_reset_the_idle_bound`,
  `a_first_frame_timeout_on_a_reused_connection_reconnects_once`.
- No public-API change in p1-contracts: `git diff origin/main -- crates/p1-contracts` is empty.
- `cargo test -p p1-provider-http` (79 lib + 4 `tests/ws.rs`) and `cargo test -p p1-provider-openai`
  (61 lib + 59 `tests/websocket.rs` + the rest) pass with `cargo clippy -p p1-provider-http --tests
  -- -D warnings` and `cargo clippy -p p1-provider-openai --tests -- -D warnings` clean; every
  bound test runs on the paused clock and wraps its wait in `tokio::time::timeout`, so a removed
  bound fails an assertion instead of hanging CI.
- Every test uses an injected clock or the paused tokio clock: no real sleep, no socket (except the
  crate's own existing loopback test for the connector, untouched), no credential file.
- Accepted with ADR-0086 (issue #298, S4.6, epic #206), with the Decision
  unchanged. On main the bounds still live where it put them: `FIRST_BYTE_TIMEOUT` and
  `STREAM_IDLE_TIMEOUT` in `crates/p1-provider-http/src/http.rs`, the SSE frame loop and
  `WAITING_NOTE_AFTER` in `drive.rs`, and `read_bounded` in `ws.rs`. The tests listed above still
  hold them. The transport broker (`broker.rs`, S4.2 #284) hands a component's lowered request
  to that same `drive` loop, so a provider component sees no time and sees bytes only after the
  loop has framed them; it cannot move a bound.
