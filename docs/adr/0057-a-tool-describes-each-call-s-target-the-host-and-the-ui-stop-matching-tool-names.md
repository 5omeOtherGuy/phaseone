---
adr: 57
title: A tool describes each call's target; the host and the UI stop matching tool names
status: accepted
date: 2026-09-23
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/research/modularity-audit-2026-09-22.md, crates/p1-contracts/src/tool.rs, crates/p1-host/src/tui.rs, crates/p1-tui/src/transcript.rs, crates/p1-workers/src/lib.rs, crates/p1-tool-delegate/src/lib.rs]
---
# ADR-0057: A tool describes each call's target; the host and the UI stop matching tool names

## Context

The modularity audit (finding 3, issue #46) found the host and the TUI classifying tool calls
by literal model-facing names (`edit | patch | write | read | shell`) and decoding those tools'
private argument keys (`file_path`, `old_string`, `command`). The patch tool is named
`apply_patch` and its input is freeform, so on the GPT environment a patch is never recognised;
any renamed `ToolFace` breaks the same matches. The first native workflow run (ADR-0053 live
check 5) found the same class in `p1-workers`: its inbox notification names the delegate
tool's model-facing name `worker_result`, which a face may change. The architecture rule is
that tools contain no UI types and the host names no tool internals: what a call touches is the
tool's knowledge, and only the tool can say it.

## Decision

1. **`p1-contracts` gains a call description.** `Tool` gets
   `fn describe(&self, call: &ToolCall) -> CallDescription` with a default implementation
   built from the declaration name alone. `CallDescription { verb: &'static str, target:
   Option<String>, edit: Option<EditPreview> }`: `verb` is a short word for the UI (`read`,
   `edit`, `run`, `search`, `finish`, `worker`…), `target` the file, directory, command or
   worker the call is about, already trimmed for display, and `edit` — filled only by a tool
   that changes one file with a known before/after (`edit`, `write`) — the `{path, old, new}`
   the approval UI shows as a diff. No argument keys leave the tool: the tool supplies what
   the UI needs, the UI never parses the tool's input.
2. **Every shipped tool implements it** from its own parsed input: read/grep → path or
   pattern; edit/write/apply_patch → the file(s) touched (apply_patch parses its freeform
   input, the same way it applies it); shell → the command's first line; finish → status;
   the worker tools → the worker id.
3. **The host and the TUI consume it.** `p1-host` task-file tracking and `p1-tui`'s call
   summaries use `effect()` + `describe()`; no code outside a tool crate names a tool or an
   argument key. The p1-tui change is coordinated with #12 (the TUI session owns `p1-tui`);
   until it lands, `p1-tui` may keep its matcher but reads the description when present.
4. **`p1-workers` takes its notification text from the tool that owns the name.** The
   delegate tool passes the model-facing name of `worker_result` (its face) into the service
   at construction; the crate-level claim "no model-facing tool is named here" becomes true.
5. **Tests**: each tool crate tests `describe()` on its real inputs; a host test renders a
   renamed face (`ToolFace`) and an `apply_patch` call and asserts both are tracked.

## Consequences

- A tool can be renamed by a face, or replaced by another crate with the same effect, without
  any change to the host or the UI: modularity as the audit measured it.
- One more trait method every tool implements (a default exists); freeform tools (patch) do a
  little parsing twice, once to describe and once to apply.

## Alternatives considered

- A display hint in `ToolDeclaration` (static): cannot name the file a particular call
  touches.
- The host parsing every tool's JSON by convention (`file_path`): the coupling being removed.

## Evidence

Merged from task/call-target (gate green on the branch merged with main; runs recorded in
`docs/dogfood/runs.jsonl`: a DeepSeek V4.1 Flash worker did three quarters of the work before
its route hit the weekly limit, a gpt-6-luna session finished it from the committed WIP and
then made the one repair the lead asked for — the edit tool supplies the `EditPreview` the
approval view shows as a diff — reviewed by the lead). Tests: `describe()` per tool crate on
real inputs including `apply_patch`'s freeform patch and a renamed face;
`crates/p1-host/tests/call_description.rs` (a renamed edit face and an `apply_patch` call are
both tracked; an edit call yields the diff approval view, `apply_patch` the permission form);
the `p1-host/src/tui` tests; a `p1-workers` test that a renamed result-tool face appears in
the parent's notification. `crates/p1-host/src/tui.rs` no longer matches `"edit" | "patch" |
"write"` nor reads `file_path`/`command`; `p1-tui`'s own matcher is left to #12.
