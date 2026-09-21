---
adr: 48
title: Providers may tell the operator something: a display-only notice event
status: proposed
date: 2026-09-21
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/websocket.md, docs/research/41-websocket.md, crates/p1-contracts/src/provider.rs]
---
# ADR-0048: Providers may tell the operator something: a display-only notice event

## Context

The Codex route speaks WebSocket by default and falls back to SSE for the rest of the process
(ADR-0047 §5). The fallback is invisible today: a provider has no way to say anything that is
not model output, so the operator cannot tell which transport a run used, and the open
long-session comparison of research #41 cannot be read from a run. Issue #6 lists this as a
harness debt.

## Decision

`StreamEvent` gains `Notice { text: String }`: operator-facing, display only. The core forwards
it as `AgentEvent::ProviderNotice { text }` and does nothing else with it — it is never part of
the history, never journalled, never sent to a model, and it does not count as activity for the
idle timeout. The text is a constant sentence chosen by the adapter plus, at most, an HTTP status
or a short token-shaped code — never a header value, body text or credential. The line renderer
prints it as one `· <text>` line on stderr; the TUI shows it as a transcript note.

First use: the Responses adapter emits `transport: WebSocket unavailable (<reason>) — using
HTTP (SSE) for the rest of this session` once, when it falls back (websocket.md §5).

## Consequences

- Every `match` on `StreamEvent` / `AgentEvent` gains an arm; the conformance suite and the
  renderers ignore or print it.
- A later retry/back-off notice can reuse the event instead of new plumbing.
- The run report cannot count notices (they are not journalled); the stderr log holds them.

## Alternatives considered

- Journal the transport per response: puts transport into replay identity, which ADR-0047 §7
  keeps out.
- Log from the adapter to stderr directly: providers own no output channel and the TUI would
  lose it.

## Evidence

To be recorded when merged: a scripted-connector test that refuses the upgrade and asserts one
`Notice` before the SSE response's events; the renderer test for the printed line.
