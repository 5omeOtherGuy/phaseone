---
adr: 220
title: Task state lives only in GitHub issues
status: accepted
date: 2026-10-18
deciders: owner
supersedes: [7]
superseded_by: []
sources: [ADR-0007, D13, packs/owner/docs/STATUS.md, "owner's repository standard 2026-10-17"]
---
# ADR-0220: Task state lives only in GitHub issues

## Context

ADR-0007 put task state on GitHub issues and gave each task its own worktree, but D13 kept
`STATUS.md` (later `packs/owner/docs/STATUS.md`) as the lead's compaction-safe resume record.
By 2026-10-17 that file still read "Status — 2026-10-06": its "OPEN, in order" list had been
resolved or moved into issues, and it pointed at paths on one workstation that neither an orb
nor a contributor can reach. The owner's repository standard (2026-10-17) rules: "No committed
status files (`STATUS`, `STATE`, `HANDOFF`, `PROGRESS`): they go stale. Work state lives in
issues and PRs." The owner applied it to this repository on 2026-10-18.

## Decision

D13's `STATUS.md` is retired: no status or resume file is committed. Work state lives in GitHub
issues and pull requests; a handoff is an issue. The rest of ADR-0007 stands: work items are
issues, owner questions are issues, and each task gets its own worktree through
`scripts/new-worktree.sh`.

## Consequences

- `packs/owner/docs/STATUS.md` is deleted. Each of its open items is held by an issue: the
  row 5 rerun on Claude by #513 (closed with its result), the ADR-0118 live check by #592
  (closed with its result), and the deferred slices I and J by #516 and #517.
- After a context compaction an agent re-reads the issues and pull requests it works on, not a
  file.
- Accepted records that still mention `STATUS.md` (ADR-0045, ADR-0153) keep their text as
  history; this record retires the file they name.

## Alternatives considered

- Keep the file and refresh it: it went stale within days before and would again, and it
  carries workstation paths.
- Move it out of the repository to the lead's notes: the issues already hold every open item,
  so a second record would only drift.

## Evidence

The deleted file's header, "# Status — 2026-10-06" (`git show 4cc262c:packs/owner/docs/STATUS.md`); `gh issue view 513`,
`gh issue view 592`, `gh issue view 516`, `gh issue view 517`.
