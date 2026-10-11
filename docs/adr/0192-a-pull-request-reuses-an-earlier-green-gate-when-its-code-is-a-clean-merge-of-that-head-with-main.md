---
adr: 192
title: A pull request reuses an earlier green gate when its code is a clean merge of that head with main
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: [issue #829, ADR-0011, ADR-0128, .github/workflows/ci.yml, scripts/gate_reuse.py]
---
# ADR-0192: A pull request reuses an earlier green gate when its code is a clean merge of that head with main

## Context

Every new head of a pull request ran the whole gate, 21 to 41 minutes for a code pull request
(16 green runs measured on 2026-10-11). A head that only merges `main`, to resolve a conflict in
a Markdown file or to catch up, paid the same, although `main` does not require an up-to-date
branch: auto-merge already lands a green head merged cleanly with a newer `main` without testing
that combination. On 2026-10-11 PR #807 re-ran the full gate twice for Markdown-only conflicts.
The owner ordered: "optimize the gates to avoid these long waits for conflicts that do not
require a full re-run of the gate."

## Decision

On a `pull_request` run, `classify` asks `scripts/gate_reuse.py` (main's copy, so a pull request
cannot change the rule that skips its own gate) whether the head may reuse an earlier green. It
may when an earlier commit C of the same pull request has a green `gate`, and the head's code is
exactly the clean three-way merge of C with the `main` commit the head is now based on
(`git merge-tree`): conflicts and differences only in Markdown. Then the Rust jobs are skipped and
`docs` (ADR check and secret scan) stands for them, as on the docs-only path. Markdown under
`environments/` is runtime input and counts as code, on both fast paths. Pushes to `main` always
run the full gate, since releases build from it (ADR-0065).

## Consequences

A pull request that merges `main` for a Markdown conflict, or only to catch up, is green in about
a minute instead of half an hour. The combined state is tested exactly as much as before: a clean
merge was already accepted untested at merge time, and the push to `main` still runs the full gate.
A conflict or any change in code, a new commit with code, or anything the script cannot read runs
the full gate. The docs-only path no longer treats `environments/**` Markdown as documentation;
such a change now runs the full gate.

## Alternatives considered

- GitHub's merge queue, which tests the combined state once per merge: it needs repository
  settings this repository does not use, and it still runs a full gate per merge.
- Requiring up-to-date branches: more full runs, not fewer.
- Skipping the gate whenever the files changed by the pull request and by `main` are disjoint: a
  rename on one side can break the other without touching its files; a clean merge with an
  earlier green head is the same standard auto-merge already applies.

## Evidence

`python3 scripts/test_gate_reuse.py` covers a clean catch-up and a Markdown conflict (reuse), a
code conflict, a code edit inside a merge, a new code commit, a prompt under `environments/` and
no earlier green (full gate). Gate durations: `gh run list --workflow ci.yml --limit 40`.
