---
adr: 131
title: Configured subagents and per-call worker options
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0131: Configured subagents and per-call worker options

## Context

The owner requested iris-agent's config-defined subagents and per-call start
options in #616, then chose the public name `subagent_type`. ADR-0129's three
separate Wasm plugins and adapted ampi prompts must remain. The existing worker
start ABI has no field for these options, and catalog-wide grant lists do not
describe the permissions of an assembled parent.

## Decision

1. Load `subagents.toml` from the first environments search directory containing
   it. Each `[[subagents]]` entry supplies `subagent_type`, `environment`, one-line
   `description`, `prompt_file`, `tools`, ordered `models`, and optional
   `allowed_children` (empty by default). Snapshot prompts with the configuration.
   Shipped entries reuse Finder, Librarian and Task's ampi prompts.
2. The generic `worker_start` exposes configured names/use cases and accepts
   model, effort, tools and system-prompt replacements, `background` (default
   true), and `isolation` (`shared` or `worktree`). Tools are intersected with the
   parent's actual assembly keys, not tool display names or the whole catalog.
   Empty tools are an explicit empty grant, with the existing host-provided finish.
3. Add a tool-only `subagents-start` WIT capability. Its JSON request is owned by
   `p1-workers`; only name/use-case metadata may execute on the restricted path.
   Existing `workers-start` records remain unchanged. Separate plugins still
   use that ABI, but the host resolves matching configured entries and enforces
   the same parent grant/child policy. With no configured entries, legacy top-level
   environment starts retain ADR-0085's catalog-based grant contract; configured
   starts are unavailable and nested starts still require configured names.
4. A leaf cannot receive delegation tools. Explicit allowed-child names plus
   delegation tools already in the parent grant permit nested starts. Each agent
   has its own scope identity, so observe/control cannot cross sibling scopes;
   completion notifications target the immediate parent. Workflow tools remain
   ungrantable. Re-grants retain the configured policy, prompt and chosen model.
5. Provider failure advances the ordered model chain by reconfiguring the same
   child and resuming its committed transcript, not replaying its task or tools.
   A model override replaces the chain. Model references use the host's existing
   environment/profile[:effort] resolution, retaining the subagent's prompt,
   context policy and tool faces across route families. Explicit effort overrides
   the companion's reasoning-off setting.
6. Worktree isolation creates a fresh detached checkout of the parent's HEAD,
   using the existing sanitized Git command path. It never reuses, resets or
   deletes another checkout or branch. The checkout is retained for inspection.

## Consequences

Operators can change subagent behavior without rebuilding components. Main agents
must themselves carry the tools they delegate; a plugin or configuration file
does not add permissions. Prompts/configuration stay local to the host. Worktrees
copy committed files only, not uncommitted edits, and require a Git repository.
Foreground starts wait without completion notifications; background starts retain
results and notify. Shared concurrency bounds still apply to nested children.

## Alternatives considered

Replacing the three plugins with one generic tool violates the owner's pluggable
module requirement. Changing child-spec WIT breaks existing components. Starting
a fresh worker after provider failure can replay committed tool side effects.
Prompt-only child restrictions do not enforce permissions.

## Evidence

Read-only donor: `~/projects/iris-agent/docs/adr/0065-kind-driven-spawn-subagent-schema.md`
and `crates/iris-subagent-runtime/`. Verification lives in `p1-workers` subagent,
fallback and scope tests; host configuration/assembly/worktree tests; and actual
`p1/worker-start` execution in the host configuration tests. The boundary allocation is
checked by `scripts/check-module-boundaries.sh`; `scripts/adr.py check` validates
this record. Builtin-agent and GitHub prerequisite records remain ADR-0129/0130.
