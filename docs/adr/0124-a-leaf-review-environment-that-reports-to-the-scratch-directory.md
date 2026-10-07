---
adr: 124
title: A leaf review environment that reports to the scratch directory
status: proposed
date: 2026-10-07
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0124: A leaf review environment that reports to the scratch directory

## Context

Issue #424. Review finders ran in p1 with the coding-agent prompt ("Implement the user's task and verify the result", "Do not ... stop at 'good enough'"), eight coordinator tools they never needed (four `worker_*`, four `workflow_*`), and a `finish` gate built for code changes. In W0 the DeepSeek finder took 20 turns after its first findings write against pi's 4, with 2 finish refusals and 2 no-op edits; the 22 runs of the 2026-09-27 round had 56 finish refusals. The refusals come from the recorded-commands rule: a session that changed files may not finish with `verification: ["none"]`, and the finders wrote their findings into `.review/out/` inside the workspace.

The host appends the worker and workflow tools to every main agent (`with_worker_tools`, ADR-0050, ADR-0053). The only switch is `[capabilities]` in `settings.toml`, which applies to every environment. An environment file has no way to say it is a leaf. ADR-0122 gave each run a scratch directory outside the workspace whose writes are not file changes.

## Decision

A review task runs in its own leaf environment, and its report goes to the run's scratch directory.

1. **Per-environment capabilities.** An environment file may carry `[capabilities]` with `workers` and `workflows` (both default `true`, unknown keys refused). It can only narrow `settings.toml`: the host appends a family only when both allow it.
2. **A shipped review environment.** `environments/deepseek-review/` uses the `deepseek` route, profile, effort and context values, with `[capabilities] workers = false, workflows = false` and the tools `read`, `grep`, `shell`, `read_output`, `write`, `finish`. Its prompt is a reviewer's: read and report, tie each finding to a line and a trigger, batch reads, write the report once, stop when every part of the task has an answer, and finish with `verification: ["none"]`. It names `{{scratch}}` as the place for the report and forbids changes to the repository.
3. **Declared output paths are the scratch directory.** A launcher that wants the report passes `--session FILE` and reads `FILE.scratch/`, and its brief names the report's file name, not a workspace path. Writes there are not file changes (ADR-0122), so the recorded-commands rule accepts `["none"]` with no new finish rule.

## Consequences

- A finder runs with eight fewer tool schemas and a prompt that does not push it to implement or keep going. Its finish is accepted at once when it changed nothing in the repository.
- Launchers must point the brief's output at the scratch directory; the 2026-09-27 review kit's briefs, which write to `.review/out/`, need that change before the measurement round.
- Other models get a review environment the same way: a directory with the same `[capabilities]` and prompt shape. Only DeepSeek's is shipped now, because the issue's measurement is a DeepSeek round.
- A review that must write inside the repository (a fix suggestion as a patch file, say) still meets the recorded-commands rule; that is a different task and keeps the coding environment.

## Alternatives considered

- **A declared-outputs list (CLI flag or worker argument) whose workspace paths do not count.** Rejected for now: it needs a second exemption beside scratch in the mutation counter and the fingerprint, for the same purpose the scratch directory already serves.
- **A "review mode" flag on existing environments.** Rejected: the prompt, tools and capabilities all differ, and an environment directory already holds exactly those.
- **Relax `finish` for every environment.** Rejected: the recorded-commands rule is right for coding tasks.

## Evidence

Issue #424; analyst report `~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md` (W0 tail and finish refusals). Code: `crates/p1-host/src/catalog/delegation.rs` `with_worker_tools`, `crates/p1-host/src/activity.rs` `verify_evidence`, ADR-0122. Tests: `crates/p1-assembly/tests/environment_capabilities.rs`, `crates/p1-host/tests/review_environment.rs`, the `with_worker_tools` unit test in `delegation.rs`.
