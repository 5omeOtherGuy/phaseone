---
adr: 100
title: Provider transport exposes bounded SSE and WebSocket seams
status: accepted
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0100: Provider transport exposes bounded SSE and WebSocket seams

## Context

Issue #470's transport work adds bounded SSE decoding, a bounded WebSocket lease queue, a shared
WebSocket policy module and a shared upgrade-refusal path. Several of these are new public
interfaces (`ws_policy`, `SseDecoder::try_push`, `SseDecoder::try_finish`,
`WsSession::lease_bounded`, `MAX_LEASE_WAITERS`, `WsConnectError::Capacity`,
`WsSendError::Capacity`), not small implementation choices, so AGENTS.md requires a recorded
decision before landing.

## Decision

Expose the provider transport's bounds as named public seams: `SseDecoder::try_push`/`try_finish`
return `Result<_, SseLimitExceeded>` with `SSE_LINE_LIMIT` = 256 KiB and `SSE_EVENT_LIMIT` =
1 MiB, and the EOF flush applies the same cumulative event bound; `WsSession::lease_bounded`
admits at most `MAX_LEASE_WAITERS` = 16 queued component calls and cancellation outranks a full
queue; `ws_policy` owns the shared once/transient/reconnect rows; and a connector capacity
failure (`WsConnectError::Capacity` → `WsSendError::Capacity`) is terminal before output, like an
oversized frame, in both the host component driver and the native parity driver.

## Consequences

A provider component and the native adapter enforce the same bounds and the same terminal
capacity class; a guest cannot make either driver retain an unbounded SSE event, wait unboundedly
for a session, or turn a capacity failure into repeated handshakes. The crate's public surface
grows by these names and must keep them stable for components.

## Alternatives considered

Keeping the bounds private and relying on the caller: rejected, because a component builds and
drives these types directly. Distinguishing a connector capacity failure by matching its message
string: rejected as fragile; a typed variant preserves the class. Leaving the EOF event bound
unenforced: rejected as an unbounded-retention hole.

## Evidence

`crates/p1-provider-http/src/sse.rs` (`eof_flush_enforces_the_event_limit`,
`a_limit_sized_line_is_accepted_with_any_line_ending`), `crates/p1-provider-http/src/ws_drive.rs`
(`a_cancelled_wait_is_cancelled_even_when_the_lease_queue_is_full`,
`capacity_handshake_failure_is_terminal_before_output`), `crates/p1-provider-http/tests/ws.rs`,
`crates/p1-provider-openai/tests/websocket.rs`;
`docs/design/websocket.md` §2/§4 and `docs/design/providers.md` SSE rules.

