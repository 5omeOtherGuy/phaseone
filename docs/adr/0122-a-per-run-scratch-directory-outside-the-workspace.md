---
adr: 122
title: A per-run scratch directory outside the workspace
status: proposed
date: 2026-10-07
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0122: A per-run scratch directory outside the workspace

## Context

Issue #457. Workers cannot write a file outside the workspace: `write` and `edit` refuse with `path escapes workspace`, and p1 names no place for scratch files. The commonest need is a PR body for `gh pr create --body-file`. In the 2026-09-28 log scan, 8 writes were refused in 7 runs, 6 of them PR bodies (`runs/j2-p1.jsonl` seq 252 `/tmp/j2-pr-body.md`, `runs/j5-p1.jsonl` seq 354, `runs/j7-p1.jsonl` seq 60). Models fell back to shell heredocs: 82 in that set, which also put large text into commands that the finish gate must later match.

Today the workspace has one root (`p1_workspace::Workspace.root`), checked in `resolve` and again by the root-descriptor walk in `commit.rs` and `read.rs`. Every successful `WritesFiles` call counts as a file change for the finish gate's evidence freshness (`ActivityLog::last_file_change`), whatever path it wrote, so a PR body written after the tests would make the test evidence stale.

## Decision

Each run gets one scratch directory outside the workspace. The agent's file tools and its shell may write there, and nothing written there counts as a change to the work.

1. **Place and lifetime.** With `--session FILE` the scratch directory is `FILE.scratch/`, created at run start (mode 0700) and kept, like the journal, so a resumed run finds its notes again. Without a session it is `$TMPDIR/p1-scratch-<random hex>/`, created at run start and removed with its contents when the run ends. One directory per run, shared by the parent and its workers.
2. **Second root.** `p1_workspace::Workspace` gains an optional scratch root (canonical). A path under it resolves exactly as a workspace path does, with the same checks against that root (no escape, no symlink out, descriptor walk from the scratch root). `read`, `write`, `edit` and `apply_patch` accept it; directory listing and the search walk stay on the workspace root. A path under neither root is still refused with `path escapes workspace`.
3. **Shell.** The host sets `P1_SCRATCH=<path>` in every shell command's environment. With `--sandbox workspace` the scratch directory is bound writable like a `--sandbox-write` path.
4. **Prompt.** A new placeholder `{{scratch}}` holds the path. Every shipped environment prompt gains one line naming it for notes and PR bodies, outside the repository.
5. **Finish gate.** A `WritesFiles` call counts as a file change only when it changed the workspace: the workspace keeps a mutation counter that every committed mutation under the workspace root bumps, and the activity log compares it before and after the call. A write under the scratch root bumps nothing. The fingerprint (ADR-0055) already ignores paths outside the workspace; when `FILE.scratch/` lies inside the workspace (a session inside it, which p1 warns about since #423), it is added to the host's ignored paths.

## Consequences

- PR bodies and notes go to a named place; no heredoc workaround is needed and no evidence goes stale for them.
- A second root widens what the file tools may touch, but only to a directory p1 itself created for this run.
- A run without `--session` loses its scratch files at exit; a run that needs them later must use `--session` or copy them.
- `Substitutions` gains a field; every struct literal that builds one changes.

## Alternatives considered

- **Let write and edit reach `/tmp`.** Rejected: it widens the tools to a shared directory other runs and users write.
- **A scratch directory inside the workspace, gitignored.** Rejected: the files show up to the model's listings and searches and to tools that do not read `.gitignore`, the problem #423 removes for journals.
- **Detect the written path from the call input in the activity log.** Rejected: it teaches the host the input shape of individual tools; the workspace counter needs no tool knowledge.

## Evidence

Issue #457 and the 2026-09-28 log scan (`~/.agents/xo/dispatch/cutover-lead/logscan/`). Code survey: `crates/p1-workspace/src/lib.rs` `Workspace::resolve`, `crates/p1-workspace/src/commit.rs` root-descriptor walk, `crates/p1-host/src/activity.rs` `last_file_change`, `crates/p1-module-runtime/src/process/sandbox.rs` writable binds, `crates/p1-assembly/src/lib.rs` `Substitutions`.
