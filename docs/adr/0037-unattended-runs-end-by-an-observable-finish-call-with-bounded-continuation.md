---
adr: 37
title: Unattended runs end by an observable finish call, with bounded continuation
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/completion.md, crates/p1-tool-finish/src/lib.rs, crates/p1-host/src/activity.rs, crates/p1-host/src/run.rs, crates/p1-host/tests/completion.rs]
---
# ADR-0037: Unattended runs end by an observable finish call, with bounded continuation

## Context

Owner failure F1: agents stop although the task authorizes them to continue — a plan, a
progress note, "shall I proceed?". The first slice answered with prompt text only. The
reviewer's plan amendment 5 narrowed what a policy may promise: it addresses premature
stopping and nothing else; it must define when continuation is justified, what verified
completion is and how a real blocker is handled, or it becomes a loop repeating failed actions;
and it must be tested on observable behaviour, not on phrases.

## Decision

Completion is an act, not a wording. An optional tool module `finish` takes
`done` (with the verification commands) or `blocked` (with what is needed). The tool checks
`done` against the session's own activity: each named command must have a recorded
successful run that is NEWER than the last file change — otherwise an ordinary tool error
tells the model what to do, inside the same turn. In a headless run whose environment has the
tool, the host judges every completed turn: accepted `done` → exit 0; accepted `blocked` →
exit 3 with the need printed, never a continuation; pending inbox or running workers → that is
WAITING, handled as before; otherwise a premature stop, answered by ONE fixed user-role message
— at most 3 per run and never twice in a row without a finished tool call in between — then
exit 4 (`stalled`). The activity log is fed from the event stream and rebuilt from the
journal on resume. No text is pattern-matched anywhere. Interactive runs are never continued.

## Consequences

- "Done" now means: verified by a command after the last change — checked by the harness.
- A real blocker ends the run with a distinct exit code an outer script can act on.
- Bounded by construction: at most three continuation turns, and no repeat without progress.
- Limits, stated: a shell command that changes files is not seen as a file change; command
  matching is exact text; a worker's turn end is still its completion (the parent verifies);
  a tool renamed across a resume is not counted as writing.
- It does not address invented constraints or forgotten decisions.

## Alternatives considered

- Detecting questions or plans in the final text: phrase matching — brittle and untestable.
- "Always continue until the model says done": the endless loop the reviewer warned about.
- A judge model deciding whether the work is complete: a second unreliable opinion, and cost.
- Core support (a new TurnEnd): not needed; a tool plus host policy keeps the core unchanged.

## Evidence

`cargo test -p p1-tool-finish`, `cargo test -p p1-host --test completion` (must-pass a0–i,
resume variants). Live result on both routes recorded at the end of
`docs/design/completion.md`, including a real rejected-then-repaired `finish`; the
continuation path itself was not triggered live.
