---
adr: 72
title: Workflow steps can run in their own git worktree
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [issue #189, docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, docs/design/workflows.md, scripts/new-worktree.sh, AGENTS.md]
---
# ADR-0072: Workflow steps can run in their own git worktree

## Context

A `p1 workflow` script fans implementers out with `parallel`, but every step runs in the
run's workspace or a path the script names (`workspace`, ADR-0053). Two implementers in
one checkout overwrite each other, and the project rule is one worktree per task
(`scripts/new-worktree.sh`, AGENTS.md). A script had no way to give a step its own tree,
nor to point a later step (a reviewer, a verifier) at exactly the tree an earlier step
worked in (issue #189).

## Decision

`agent(prompt, #{ worktree: "<slug>" })` runs the step in its own git worktree.

1. **The option.** The slug is lowercase ASCII letters, digits and `-`, starts and ends
   with a letter or digit, at most 64 characters; anything else is a script error, and so
   is `worktree` together with `workspace`. `p1-workflow` stays git-free: it only carries
   the slug and the base as strings (`StepRequest.worktree`, `StepRequest.base`); every git
   call is native host code (`crates/p1-host/src/worktree.rs`), through the `git` CLI — no
   new crate.
2. **The base.** When the host starts a run — `p1 workflow run` and the agent's
   `workflow_start` alike — it resolves `git rev-parse HEAD` of the run's workspace (for the
   tool, the workspace its steps fall back to) and carries it as `StartRequest.base`,
   journalled on the `started` line. On `resume_from` the resumed run's recorded base wins
   over a fresh one, so a resumed run makes its missing trees from the same commit. No base
   (not a git repository) fails only the steps that ask for a worktree, with `worktree: …`.
3. **The tree.** The path is `<parent of the main worktree>/<main worktree name>-<slug>`
   (the main worktree is the first entry of `git worktree list --porcelain`), exactly what
   `scripts/new-worktree.sh` makes, on branch `task/<slug>`. A registered worktree at that
   path on `task/<slug>` is reused untouched — that is resume and repair; a path that exists
   otherwise is an error naming path and branch; an existing branch without a worktree is
   attached (`git worktree add <path> task/<slug>`); otherwise a new branch is made from
   the base (`git worktree add -b task/<slug> <path> <base>`).
4. **Refusal.** The host keeps the set of worktree paths held by running steps under one
   lock, which also serialises `git worktree add`. A step whose tree is held fails at once,
   before dispatch, with `worktree_busy: <slug>`. The hold lasts the whole step — its
   fallback links and its repair turn — and is released on every end.
5. **The result.** The envelope gets `worktree: {path, branch, head}` (head = the tree's
   `HEAD` after the step ended); the script reads `r.worktree.path` and can pass it as a
   later step's `workspace`. A replayed step returns its recorded envelope unchanged.
6. **Never deleting.** The host never deletes, resets, cleans or forces anything. A
   finished tree is removed with `git worktree remove` by the existing rule for every
   worktree (AGENTS.md): only when its work is merged or pushed and its claim released.

## Consequences

Parallel implementers never share a checkout, and a reviewer step can be pointed at
exactly the tree it reviews. Trees and branches accumulate until someone removes them —
deliberately: a step's tree may hold the only copy of unfinished work. The `StepRunner`
trait gains a `worktree` method with a refusing default, so another runner makes no trees
until it chooses to. The busy set is per host process: two separate `p1` processes are
not kept apart on one tree (git itself refuses a second worktree of one branch).

## Alternatives considered

- Git inside `p1-workflow`: rejected — the engine knows no host, tool or process
  (ADR-0053); strings keep it testable with a fake runner.
- A worktree per step by default, or pools of trees: rejected — most steps read, and the
  script is the one that knows which steps edit.
- Resetting or recreating a tree that is in the way: rejected — it could destroy a
  previous step's uncommitted work.
- A git library crate: rejected — no new crate; the `git` CLI is what every script here
  already uses.

## Evidence

- `crates/p1-workflow/tests/worktree.rs`: option parsing, the base in every
  `StepRequest`, the recorded base kept on resume, the refusal before dispatch, replay.
- `crates/p1-host/src/worktree.rs` tests, against a temp git repository: create from the
  base after `HEAD` moved, reuse keeping an uncommitted file, a foreign path refused, an
  existing branch attached, a held tree refused and released.
- `crates/p1-host/tests/workflow_run.rs` `two_steps_run_in_two_worktrees`: one run, two
  steps, two trees with distinct paths and heads.
