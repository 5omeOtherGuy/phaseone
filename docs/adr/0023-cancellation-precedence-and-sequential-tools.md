---
adr: 23
title: Cancellation precedence and sequential tool execution
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D18, docs/design/core.md, docs/design/seams.md]
---
# ADR-0023: Cancellation precedence and sequential tool execution

## Context

D18 (lead): a dropped tool future leaves its side effects unknown, so the core awaits a
running tool on cancellation. `core.md` ruling R1 fixes the precedence when a terminal stream
event and cancellation arrive together. `seams.md` section 6 keeps the tool boundary simple.

## Decision

Whenever the core is about to wait on the provider or start a tool call, it checks
`cancel` first. If cancellation has fired, it wins even when a stream event (including
`Finished(Completed)`) is ready: that response is recorded as
`AssistantInterrupted{Cancelled}` and adds nothing to history. Once `AssistantCompleted` is
committed the response stands and cancellation takes effect at the tool boundary. Tools run
strictly sequentially, in block order, and the core awaits a running tool rather than dropping
it.

## Consequences

No future is abandoned with unreported side effects, and history stays well-formed after
cancellation. Parallel tool execution is deferred; it is an optimisation with ordering
consequences and is not needed to prove the architecture.

## Alternatives considered

Parallel tool execution (D18: "not needed to prove the architecture"); dropping the
running tool future on cancellation (rejected because side effects would be unknown).

## Evidence

`docs/design/core.md` ruling R1 and sections 4 and 6. `docs/SLICE-REPORT.md` acceptance 1
(112 core tests against fakes) and acceptance 4 (conformance cancellation checks
`cancel_before_first_byte` and `cancel_mid_stream`).
