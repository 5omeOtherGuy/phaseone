---
adr: 150
title: Task workers derive their prompt from the parent agent
status: accepted
date: 2026-10-09
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0150: Task workers derive their prompt from the parent agent

## Context

The owner requested: "Task agent and main agent prompt should be the same" and
clarified that p1 should reproduce ampi's difference between those prompts.
ampi derives Task from the parent's mode body, rebuilds tool-dependent guidance
for the worker, and appends `buildTaskWorkerRoleBlock()`. p1 instead maintained
an independent Task coding prompt alongside the main environments' templates.

## Decision

Add optional `inherit_prompt` to configured subagents, defaulting to false and
enabled for the shipped Task entry. Its `prompt_file` supplies only the worker
role and p1 completion instructions; the host prepends the actual parent's
unrendered environment template. Child assembly renders the combined template
against the child's own tool grant, faces, workspace, date and scratch directory.
An explicit per-call `system_prompt` remains a complete replacement.

## Consequences

- Main guidance has one source for each environment; Task no longer duplicates it.
- The parent template is captured with the assembling parent's tools, including
  model switches and reloads. Existing children and their re-grants keep that
  snapshot; they do not reread a newer parent template.
- Finder and Librarian remain standalone prompts. Model/effort defaults, grants,
  nested-worker restrictions, the module ABI and completion policy do not change.
- Unlike ampi's preserved-context tail, p1's private `--instructions` and skill
  index remain main-only (`crates/p1-host/src/instructions.rs`). This change shares
  environment guidance, not private launch context or conversation history.
- Launching the Task environment directly without configured dispatch supplies
  its role/completion prompt only: no parent exists to inherit from.

## Alternatives considered

Copy the main prompt into Task: rejected because copies drift and cannot follow
the parent's environment. Copy the rendered parent prompt: rejected because it
leaks parent-only tool names and workspace substitutions into narrower workers.

## Evidence

Donor: `5omeOtherGuy/ampi`,
`src/extensions/ampi-core/subagent-prompt-assembly.ts` (`assembleModeDerivedSurface`),
`src/extensions/ampi-workers/builtin-workers/task.ts` (`buildTaskWorkerSystemPrompt`),
and `src/extensions/ampi-workers/profiles/prompts.ts` (`buildTaskWorkerRoleBlock`).
The existing p1 Task role text already adapts that donor block.

`catalog::subagents::tests::mode_derived_workers_share_parent_guidance_but_render_their_own_tools`
checks inherited guidance, worker-only tool faces, parent-only section removal,
workspace substitution, re-rendering after a grant change and full prompt replacement.
The shipped configuration test pins Task as the only inheriting companion;
`lead_prompt_coherence.rs` sweeps shipped templates over every tool subset.
