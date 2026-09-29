---
adr: 98
title: Write-gate waiter count is public test observability
status: proposed
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0098: Write-gate waiter count is public test observability

## Context

`WriteGate` serializes file mutations across agents and is shared across crates.
A regression that proves a mutation refused after it was queued, rather than
mutated, must observe the moment a writer is parked on the gate. A sleep is not
allowed (the project forbids sleep-based timing assertions), so the test needs a
read-only signal from the gate itself. The branch exposes
`WriteGate::waiting_writers` (synchronous plus owned waiters) for that purpose,
which widens the `p1-workspace` public interface. AGENTS.md requires an ADR for a
changed interface.

## Decision

Keep `WriteGate::waiting_writers` public as a read-only observability accessor:
it returns the number of callers parked on the gate, synchronous and owned, so a
test in another crate can synchronize on a queued writer without sleeping. It is
not a synchronization primitive and grants no authority over the gate.

## Consequences

Consumers can reason about and test gate queueing deterministically across crate
boundaries. The cost is one more public read-only method on `p1-workspace`; its
return value is a count only, never a guard, so it cannot be used to take or
release the gate.

## Alternatives considered

Make it `pub(crate)`: rejected because the native `apply_patch` regression lives
in `p1-tool-patch` and cannot reach a crate-private helper. Move or duplicate the
test into `p1-workspace`: rejected as a larger change that would not exercise the
native patch's own wait path. Synchronize with sleeps: rejected by the project's
no-sleep rule.

## Evidence

`crates/p1-workspace/src/gate.rs` (`WriteGate::waiting_writers`);
`crates/p1-tool-patch/src/lib.rs` test
`a_patch_cancelled_while_queued_on_the_gate_does_not_write`; queueing counters in
`State::sync_waiters` and `State::wakers`.
