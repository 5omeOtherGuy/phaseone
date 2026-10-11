---
adr: 176
title: The three-issue limit applies to local workers; orbs are limited only by disjoint files
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: [AGENTS.md, docs/adr/0128-fast-lane-one-worker-per-issue-review-before-the-push-a-docs-only-gate-path-and-ssd-only-builds.md]
---
# ADR-0176: The three-issue limit applies to local workers; orbs are limited only by disjoint files

## Context

ADR-0128 point 1 says up to three issues run at once on disjoint files. It states no reason
for the number; when it was written every worker ran on the workstation, in a local worktree
with its own target, and ADR-0128 keeps ADR-0119's three-build limit, which matches the
machine's three `rustc-serial` slots. Since 2026-10-10 p1 implementation runs on Amp orbs,
each building on its own cloud machine, so the workstation's memory, SSD and CPU are not
involved. The owner asked on 2026-10-11 whether the limit was aimed at local agents (memory,
SSD, CPU) and, on the answer that it was, approved recording that orbs are limited only by
disjoint files ("y please").

## Decision

The three-issues-at-once limit applies to workers that build on the workstation. Orbs have no
count limit; they must still own disjoint files, as ADR-0128 requires, so their pull requests
do not conflict. The rest of ADR-0128 stands.

## Consequences

More issues can run in parallel on orbs. Merge conflicts in shared files (for example the ADR
index in `docs/adr/README.md`) still arrive as each pull request lands and are resolved by
merging main. Many pull requests gating at once may queue on GitHub's concurrent-job limit;
that slows gates, it does not break them. Local builds keep the machine limits in AGENTS.md
(Build) and ADR-0119.

## Alternatives considered

Keep three for orbs too: rejected by the owner; the limit protected the workstation, which
orbs do not use. A fixed higher number for orbs: no measured basis for any number.

## Evidence

Owner exchange of 2026-10-11 in the orb dispatcher thread; seven orbs ran at once that night
(#683, #62, #641, #74, #77, #80, #67), each on disjoint files. ADR-0128 Context and point 1;
ADR-0119 (three-build limit).
