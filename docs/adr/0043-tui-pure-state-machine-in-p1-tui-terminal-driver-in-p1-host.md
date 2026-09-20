---
adr: 43
title: TUI: pure state machine in p1-tui, terminal driver in p1-host
status: proposed
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [issue #12 plan comment and lead acceptance]
---
# ADR-0043: TUI: pure state machine in p1-tui, terminal driver in p1-host

## Context

The TUI (issue #12) needs to be testable without a TTY (CI has none) and must not
couple p1's module graph to a UI. The handoff fixed the dependency rule: p1-tui
depends on `p1-contracts`, `ratatui`, `crossterm` (pre-approved) plus `tokio`
(already in the workspace) — never on a provider, a tool crate, or p1-core
internals. The iris donor's 41k-line UI (dual backends, theme system, markdown
engine) was evaluated and deliberately not carried over; the p1 host is 4.6k
lines and the TUI is capped at 6k.

## Decision

`p1-tui` is a **pure state machine and cell renderer**: `Screen` state in,
styled `ratatui` lines out, crossterm key events mapped to commands. No async,
no agent, no terminal. The terminal driver (agent ownership, channels, the
select loop) is one module, `p1-host/src/tui.rs`, behind the host's `FrontEnd`
seam; the line renderer stays the default forever. The TUI's authorization is
its own `AuthorizationPolicy` (`TuiPolicy`) parked on the screen; observation
is its own `EventSink` (`TuiSink`) with events stamped in milliseconds so the
renderers run on injected time.

## Consequences

Every screen is snapshot-tested against `ratatui::TestBackend` at 120x40 and
80x24; the closed palette and colour-stripped legibility are enforced as tests.
The driver is the only async part and is channel-tested without a TTY. Worker
events arrive tagged by id over the same channel. The cost: the driver's
`pump` (pinned turn future inside the select loop) is subtler than a plain
task — forced by the seam's borrowed `&mut Agent`.

## Alternatives considered

Driver-in-p1-tui (depends on p1-core's public API): rejected on review (the
composition root owns agent lifetime; p1-tui gains nothing). Iris's dual
backends and theme trait: dropped as out of scale for p1.

## Evidence

`crates/p1-tui/` (snapshot suite in `tests/snapshots.rs`, scripted stream in
`tests/m2_stream.rs`); `crates/p1-host/src/tui.rs`; live verification in tmux
at 120x44 and 80x24 (captures in `.shots/`, noted on issue #12).
