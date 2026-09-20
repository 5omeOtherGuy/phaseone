---
adr: 15
title: Send-capable boxed-future contracts, one owner per agent
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D17, docs/design/seams.md, docs/design/core.md]
---
# ADR-0015: Send-capable boxed-future contracts, one owner per agent

## Context

D17 (lead): the contract choices are "forced by a difference between the two real
routes". `seams.md` section 9 proposed Send-capable public async interfaces and one owner per
agent's mutable state, without inheriting Iris's local-task restriction.

## Decision

Public async contracts return boxed `Send` futures; `async-trait` is not used. Each
agent's mutable state has a single owner; the core spawns no tasks and needs no particular
runtime flavour. `Agent` is `Send`, `run_turn` returns a `Send` future, and `Inbox` is
`Send + Sync + Clone`.

## Consequences

The core can be embedded freely and agents can be scheduled across threads, while
concurrency still comes from async I/O rather than `Arc<Mutex<_>>` around all state. Some
donor code is `!Send` and must sit behind a private adapter rather than become a public
limitation.

## Alternatives considered

Inheriting Iris's `LocalSet`/`!Send` restriction as a public property, or adding
`async-trait` for these contracts. `docs/design/design-summary.md` ("Tradeoffs we are
choosing") chooses Send-capable interfaces over the local-task restriction; D17 chooses boxed
`Send` futures over `async-trait`.

## Evidence

`docs/design/core.md` section 8 ("Threading") states the guarantees, and the 112 core
tests in `docs/SLICE-REPORT.md` acceptance 1 run against scripted fakes. Commit b2a81ba added
the contracts and core API stub.
