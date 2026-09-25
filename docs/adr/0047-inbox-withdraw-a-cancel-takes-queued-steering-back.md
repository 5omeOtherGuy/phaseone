---
adr: 47
title: Inbox withdraw: a cancel takes queued steering back
status: proposed
date: 2026-09-22
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0047: Inbox withdraw: a cancel takes queued steering back

## Context

The TUI sends operator steering straight into the agent's `Inbox`, which hands it
to the model at the next safe boundary. When the operator pressed Ctrl+C with
steering still queued, the host cleared only its own display: the message stayed
in the inbox, the idle loop saw `inbox_ready()` and started a new inbox turn that
ran tools right after the cancel. Four independent reviewers and testers of the
finishing pass reproduced it (fixture, `slow` / `cancel` scenarios; journal shows
`assistant_interrupted` then `inbox steering` then a new `tool_started`). The
core offered no way to take a message back.

## Decision

`p1_core::Inbox` gains `withdraw(kind) -> Vec<String>`: it removes and returns,
in send order, every not-yet-delivered message of that kind and leaves other
kinds queued. The TUI calls it for `InboxKind::Steering` when a turn ends
cancelled and returns the texts to the composer; worker notifications stay.

## Consequences

Ctrl+C means stop again: no turn starts from steering typed for cancelled work,
and nothing the operator typed is lost. The core stays UI-agnostic (the method
names no front end). A message already drained at a boundary cannot be withdrawn;
that is correct, because the model has seen it.

## Alternatives considered

Keeping steering in the host and sending it only at a boundary: the host cannot
see the core's boundaries, so delivery would be late or racy. Suppressing the idle
`inbox_ready` drain after a cancel: the stale steering would then be injected into
the next prompt's turn instead.

## Evidence

`crates/p1-core/tests/impl_core.rs::withdraw_takes_back_only_undelivered_messages_of_one_kind`;
host driver test `a_cancel_returns_queued_steering_to_the_composer` in
`crates/p1-host/src/tui/tests.rs`; discovery evidence
`.verification/finish/evidence/{r4-driver,t1-compose,r1-composer}/`.
