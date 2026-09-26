---
title: Asynchronous reconfiguration commits before it installs
status: proposed
date: 2026-09-26
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0049, ADR-0078, ADR-0080, ADR-0084]
---
# ADR-draft: Asynchronous reconfiguration commits before it installs

## Context

ADR-0049 made `Agent::reconfigure` the one operation that switches a running agent to
another assembly between turns. It is synchronous: it validates the candidate against
the current history, installs the parts, and clears the agent's "environment committed"
flag, so the next turn writes the new `Environment` record lazily before its input.

ADR-0084 items 3 and 6 reload policies through the same operation and require more: the
complete candidate is validated, committed as one environment record, and installed
with no await between the commit and the installation; a failure leaves the current
assembly intact and is reported. ADR-0078 places every replacement between complete
turns. With the lazy commit, a switched assembly is installed while the journal still
names the old one, a failing journal is only noticed by the next turn, and the
authorization policy, which ADR-0084 reloads, cannot be replaced at all because ADR-0049
kept it as a session part.

This decision amends ADR-0049's lazy environment commit for the reconfigure path only.
ADR-0049 stays accepted and is cited here, not superseded. The first turn's lazy
commit and resume are unchanged. ADR-0080's version-2 journal identity line stays the
host's and p1-journal's: the core writes the same `Environment` record body it always
wrote and knows no package, digest or WIT.

## Decision

1. `Agent::reconfigure(&mut self, next: Reconfiguration) -> Result<(), ReconfigureError>`
   is `async`. `&mut self` means no turn is running; queueing a request made while the
   session is busy until a boundary between complete turns is the host's job
   (ADR-0084 item 3).
2. The operation runs in this order:
   1. The whole candidate is validated against the current history before anything is
      written: duplicate assembled tool names first, then the candidate provider's
      `validate` of the request that history would make. This is the same check
      construction and resume run.
   2. The candidate's complete `Environment` record, with the fields the first turn's
      lazy commit writes and computed from the candidate, is committed at the next
      sequence number, and the call awaits it.
   3. Once that commit has returned, every part is installed synchronously and the
      environment is marked committed, so the next turn writes no second `Environment`.
      Nothing awaits between the commit's return and the installation. A caller that
      drops the future while the commit is pending leaves the old assembly installed —
      but it cannot conclude that nothing was committed. The session store's commit is
      a write on a blocking thread (`JsonlJournal` uses `spawn_blocking`), and the
      runtime does not cancel such a task when the awaited future is dropped: the
      record may already be durable and the store's own sequence ahead of the agent's,
      so the journal can name an assembly that never answered, which ADR-0078 §4
      forbids. A caller that can abort must treat the outcome as unknown and resume,
      never retry on the assumption that the old assembly is also the committed one.
3. `ReconfigureError::Rejected(BuildError)` reports a failed validation. Nothing is
   committed and nothing is installed. Its message is the `BuildError`'s, unchanged.
4. `ReconfigureError::CommitFailed(message)` reports a failed commit. Nothing is
   installed, the sequence number does not advance, and the old parts and the old
   "environment committed" state stay. A later reconfigure with a working journal
   commits at the next dense sequence number.
5. `Reconfiguration` gains `authorization: Option<Arc<dyn AuthorizationPolicy>>`, where
   `None` keeps the current policy. The field is an `Option` rather than a required
   `Arc` because the core hands none of its parts out, and a model switch or a worker
   re-grant does not own the session's policy. Making them hold it only to pass it back
   would widen the core's API for no behaviour. The journal and the event sink belong to
   the session and are never replaced.
6. A candidate equal in content to the current environment still commits one
   `Environment` record. The call is an explicit change, and comparing environments
   would need an equality over providers and tools that the core does not have.

## Consequences

- When `reconfigure` returns `Ok`, the journal already carries the `Environment` record
  of the assembly that answers every later call: its provider route, system prompt,
  tools and options. The record body names no policy or module identity, so after a
  policy reload it does not by itself show which authorization policy decided the
  later calls. That identity is ADR-0080's assembly line, written by the host and
  p1-journal; its writer lands with S1.9.
- A journal failure surfaces at the switch and not at the next turn. The old assembly
  keeps answering and no sequence number is used.
- Callers await the operation. The worker task and the host's model-switch path await
  it. The TUI applies an idle `/model` or `/effort` from its loop, where it can await,
  and no longer from inside the key handler. The switch path no longer holds its session
  lock across the call.
- A switch made before the first turn commits the candidate at sequence 0. The
  construction environment, which never answered, is never written.
- An unchanged candidate adds one record per explicit reconfigure. This is the cost of
  item 6.

## Alternatives considered

- **Keep the lazy commit and only make installation atomic.** Rejected: the journal
  would lag the installed assembly for a whole boundary. A commit failure would surface
  in an unrelated turn, and ADR-0084 item 3 could not be met.
- **Install first, then commit and roll back on failure.** Rejected: between install
  and rollback the agent would hold an assembly the journal never named, and a dropped
  future would leave it installed.
- **A required `authorization: Arc<dyn AuthorizationPolicy>`.** Rejected: every model
  switch would have to obtain the session's current policy. The core would need an
  accessor for that, or the host would need to keep a second copy, and neither adds
  behaviour.
- **Treat an unchanged candidate as a no-op.** Rejected: it needs content equality
  over trait objects, and it hides an explicit operator action from the journal.
