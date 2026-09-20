---
adr: 32
title: Agents sharing a directory serialize their file mutations
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/review-2026-09-20-dispositions.md, docs/design/seams.md, docs/design/delegation.md, crates/p1-workspace/src/gate.rs, crates/p1-tool-tests/tests/shared_workspace.rs]
---
# ADR-0032: Agents sharing a directory serialize their file mutations

## Context

`seams.md §8` requires conflicting parallel writes to be serialized or isolated. The first
slice weakened that: `delegation.md` (and a consequence noted in ADR-0025) left concurrent
writers to the model. The independent review of 2026-09-20 (departure D-A) called this a
material weakening, and it is a real defect, not only a wording problem: each agent has its
own observation registry and the mutating tools run on blocking threads, so a parent and a
worker could both pass "unchanged since I read it" and both write. The lead's stress test
lost an update within the first rounds (`parent 3` missing) without the fix. The reviewer
also noted that a worktree pool is not needed just to restore the invariant, and that
per-path grants cannot constrain shell commands without an execution boundary.

## Decision

Serialize, now; isolate, later. All agents assembled from one `Catalog` — a parent and its
workers — share one `p1_workspace::WriteGate`. `edit`, `write` and `apply_patch` hold it from
reading the file's current contents until their write is recorded, so check-and-write is one
step across agents: the second writer is checked against what the first one wrote and gets
the ordinary stale-file error (or, for `apply_patch`, its context lines are matched against
the new contents) instead of overwriting. Observation registries stay per agent.

The guarantee is stated with its limit: it covers the file tools. A SHELL command's writes
are neither gated nor checked — a later file-tool mutation notices them, but two agents whose
commands rewrite the same files can still lose updates. Only a separate working directory
isolates that; until child isolation exists, prompts keep telling the model not to give
overlapping files to workers, and real tasks are dogfooded in disposable worktrees.

## Consequences

- No lost update between file-tool mutations of any agents of one process.
- Mutations are serialized process-wide, even across different directories: the critical
  section is a few file operations, so this costs nothing measurable and needs no path map.
- The gate is held on a blocking thread, never across a model or network wait.
- Another p1 PROCESS in the same directory is not covered (no cross-process lock); its
  changes are still detected by the staleness check, minus a small check-to-rename window.
- Isolation for children (own worktree) remains future work and is the only answer for
  shell-level conflicts.

## Alternatives considered

- A lock per path: more state, and `apply_patch` touches several files (lock ordering) — no
  benefit while the critical section is this short.
- One shared observation registry: would hide one agent's change from the other's staleness
  check — the opposite of what is wanted.
- Worktree per child now: the right isolation, but merging a child's work back is a product
  decision of its own; not required to restore the invariant.
- Gating shell commands: would serialize builds and test runs of all agents for up to an
  hour each.

## Evidence

`cargo test -p p1-tool-tests --test shared_workspace`: a queued edit sees the write that
landed before it; three agents (two `edit`, one `apply_patch`) × 150 rounds on one file lose
nothing; two agents creating the same file — exactly one wins, the other is told to read it.
The last two go red when the agents do not share the gate. Wiring:
`cargo test -p p1-assembly --test lead_write_gate`.
