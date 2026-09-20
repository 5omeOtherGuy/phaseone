---
adr: 25
title: Always-on workspace confinement and read-before-mutate, with apply_patch exempt
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/tools.md, docs/design/seams.md]
---
# ADR-0025: Always-on workspace confinement and read-before-mutate, with apply_patch exempt

## Context

`seams.md` section 4: a file tool's "confinement/atomic-write and read-before-mutate
invariants belong in the tool/backend even when interactive approval is disabled."
`docs/design/tools.md` specifies the checks and the apply_patch exception.

## Decision

Every path a file tool touches resolves inside the workspace root after symlink
resolution, always — not a permission setting. `edit` and `write` refuse a file this agent has
never observed or that changed since it was observed. `apply_patch` is exempt because its
hunks must match the file's current contents (its own staleness check) and the GPT environment
reads files through `shell`, which the registry cannot see; it still records what it wrote.
apply_patch follows Codex semantics: the V4A lark grammar, full validation before anything is
written, and the Codex fuzz ladder (exact, then trailing-whitespace-insensitive, then
whitespace-insensitive).

## Consequences

`..` escapes, absolute outside paths and symlinks out of the root are all rejected; the
GPT route can patch without the observed-file registry. Concurrent writers remain the model's
responsibility in this slice; worktree isolation and granular grants are future work.

## Alternatives considered

Making confinement a permission setting (rejected: it is always on); Iris's fuzzy edit
matching (left behind per `docs/SLICE-REPORT.md`).

## Evidence

`docs/design/tools.md` ("Confinement invariant", "Read-before-mutate invariant", the
apply_patch section). `docs/SLICE-REPORT.md` records that the lead's adversarial tests found
the addition-only patch hunk placement defect. Commits bd66067 (workspace/read/edit/write)
and 3a8e177 (grep/shell/apply_patch).
