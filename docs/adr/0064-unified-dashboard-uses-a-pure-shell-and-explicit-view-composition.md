---
adr: 64
title: Unified dashboard uses a pure shell and explicit view composition
status: accepted
date: 2026-09-24
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [issue #89, docs/design/unified-dashboard.md]
---
# ADR-0064: Unified dashboard uses a pure shell and explicit view composition

## Context

Quota watch and worker activity already render terminal views, but do not compose
as a navigable application. ADR-0043 assigns pure state/cells to the TUI and async
terminal ownership to the host. ADR-0056 preserves the SLAB visual contract.
Unifying navigation must not turn the agent core into a UI or data integration hub.

## Decision

Propose an explicit view interface and a pure navigation/rendering shell. The host
supplies concrete views using ordinary constructors. The framework owns selection
and viewport composition, not probes, worker services, authentication, storage or
terminal lifecycle. Dashboard adapters are separate from framework policy.

The first bounded proof lives in `p1-tui` with a read-only adapter over the existing
worker renderer. It does not replace the live TUI. Host composition will reuse quota
snapshot rendering without adding probe dependencies to the framework. Independently
selectable modules can then be extracted into crates at the proven boundary; no
dynamic loading or global service registry is introduced.

Brain and board retain their repositories and data. Their future adapters consume
lead-agreed read-only snapshots with explicit unknown/stale/unavailable states.
Neither `p1-core` nor the minimal loop contracts gain dashboard types.

## Consequences

- Pure navigation and bounded rendering can be tested without a TTY or network.
- Existing worker-pane actions remain in their current live host; read-only
  dashboard views must not advertise controls they cannot execute.
- Quota/worker integration, host lifetime wiring and visual acceptance remain
  subsequent increments; a synthetic preview is not evidence of live integration.
- Existing SLAB snapshots remain unchanged. A concrete visual-design milestone
  requires real terminal capture and lead inspection before acceptance.
- No new dependencies are needed for the first proof.

## Alternatives considered

- Replace the agent driver or put all data acquisition in the shell: rejected;
  violates lifecycle and ownership seams.
- Invent a plugin registry or shared cross-project store: rejected as unnecessary
  generality and ownership transfer.
- Build a second renderer for worker rows: rejected; reuse the shipped renderer.

## Evidence

Inventory and incremental acceptance plan: `docs/design/unified-dashboard.md`.
This ADR remains proposed until implementation is independently verified and merged.
