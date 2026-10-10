---
adr: 157
title: Repeated tool reminders are an advisory context-policy adapter
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0157: Repeated tool reminders are an advisory context-policy adapter

## Context

Issue #624 asks for dsh's repeated identical tool call reminder: consecutive calls
with the same tool and recursively key-sorted JSON arguments, at counts 3, 5 and 8,
a 500-character argument preview, reset on a different call or new user prompt,
and no blocking. ADR-0153 requires the capability to live behind a port in its
own module. The lead approved a separate adapter behind `ContextPolicy`.

## Decision

Build `p1-repeat-tool-reminder` as a native `ContextPolicy` decorator, composed by
the host for each parent and child agent, with or without summarizing context.
It inserts a labelled `InboxKind::Notification` after the corresponding tool
result through the existing journalled history-replacement contract. It never
participates in authorization or dispatch, and leaves tool results unchanged.
The host excludes reminder-only insertions (and trims plus reminders) from its
idle-summary budget; real summaries still count even when they carry a reminder.

Identity excludes call IDs, preserves array order, and distinguishes JSON from
freeform text. Invalid JSON and freeform inputs compare as exact raw strings.
A new user message or steering clears the chain; other notifications do not.
Assistant call order determines the chain even when results complete out of order.

## Consequences

No change to `p1-core`, provider wires, or the contracts crate is needed. The
adapter tracks installed history and the live chain per agent; automatic and
manual compaction preserve that chain, while a fresh adapter reconstructs from
the available history since the most recent user prompt. Reconstruction cannot
recover calls removed by an earlier process's compaction. Existing reminders
adjacent to their results are not duplicated on resume. A summary that removes
a freshly completed result receives its fresh reminder at the end instead.

The adapter holds a history snapshot, adding memory proportional to current
context. It forwards inner context errors and compaction results unchanged;
the reminder itself has no error or blocking outcome. Reminders are visible
only at the next context boundary, after the current tool batch has completed.

## Alternatives considered

Detection inside the core loop or a provider would fuse a protected variation
point. Authorization interception could block calls and cannot insert context
after their results. A new interception port is unnecessary because the existing
context-policy contract already supports persistent labelled notifications.

## Evidence

`cargo test -p p1-repeat-tool-reminder` exercises thresholds and their neighbours,
nested key sorting, array order, resets, retries/resume, result-order independence,
compaction and Unicode preview bounds through `ContextPolicy`. Tests have no
network, timers or sleeps. Expected cost reduction is inferred, not measured.
The host's `repeat_reminders_do_not_exhaust_the_headless_idle_summary_budget`
test executes ten calls with a one-summary stall budget, then checks that an
actual summary still trips that guard.
