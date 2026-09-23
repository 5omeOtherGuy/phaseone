---
adr: 59
title: A tool describes its results and its destructiveness; the host describer keeps no tool-name table
status: proposed
date: 2026-09-23
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/adr/0057-a-tool-describes-each-call-s-target-the-host-and-the-ui-stop-matching-tool-names.md, crates/p1-host/src/tui/describer.rs, docs/design/tui/slab/TUI-HANDOFF.md, crates/p1-contracts/src/tool.rs]
---
# ADR-0059: A tool describes its results and its destructiveness; the host describer keeps no tool-name table

## Context

ADR-0057 gave every tool `describe(call) -> CallDescription { verb, target, edit }` and removed
the host's tool-name matching from `tui.rs`. The SLAB TUI (#57, #59) had meanwhile added
`crates/p1-host/src/tui/describer.rs`, which matches `read | write | edit | apply_patch | grep |
shell` for both call rows and result rows and renders each tool's RESULT from that tool's private
output shape (issue #60). A renamed face or a new tool crate gets no row. The TUI handoff §14.9
also asks for tool-provided destructiveness so the UI's destructive floor stops being a pattern
match on commands. Both are the same principle as ADR-0057: the tool supplies what the UI needs;
the host and the UI never know tool internals.

## Decision

1. **`Tool::describe_result`.** `p1-contracts` gains `fn describe_result(&self, call:
   &ToolCall, result: &ToolResultItem) -> ResultDescription` with a default that takes the
   result text's first line. `ResultDescription { summary: String, detail: Option<ResultDetail> }`
   and `ResultDetail::{ Diff { path, before, after }, Command { exit_code: Option<i32>,
   elapsed_ms: Option<u64>, tail: Vec<String> }, Matches { count: usize, files: Vec<String> },
   Files { paths: Vec<String> }, Text(String) }`. Every shipped tool implements it from its own
   output: read → `Text`/`Files`, grep → `Matches`, edit/write → `Diff` (the tool already knows
   before and after), apply_patch → `Files` (the files it touched), shell → `Command`, finish,
   worker and workflow tools → `Text`.
2. **`CallDescription.destructive: bool`.** The tool's own judgement of a call before it runs:
   shell marks `rm -rf`-class commands, `git push --force`, `git reset --hard`, writes through
   redirection outside the workspace; edit/write/apply_patch mark writes outside the workspace;
   everything else is `false`. The TUI's destructive floor reads this flag and no longer
   pattern-matches; the host's approval view shows it.
3. **`describer.rs` renders only from `describe()` and `describe_result()`.** The tool-name
   match and every private-shape decode go; the host looks a call's tool up by name in the
   assembled tools (the name is the key, never a classifier) and asks it. Rows for tools the
   describer has never heard of come out of the defaults.
4. **The SLAB oracle stays the contract.** Every screen in `crates/p1-tui/tests/common/slab.rs`
   keeps matching; a row that must change is a #12 coordination, not a silent edit. `p1-tui`
   itself is not touched.
5. **Tests.** Each tool crate tests `describe_result` on its real outputs and `destructive` on
   real inputs; a host test renders a renamed face and a tool unknown to the describer and
   asserts both get rows from the defaults.

## Consequences

- The last tool-name table outside the tool crates is gone; a new tool crate shows up in the
  TUI with sensible rows and can opt into rich ones.
- Destructiveness is decided where the command is understood, with a test per case, instead of
  in a UI regex.
- Every tool implements two more methods (defaults exist); the edit and patch tools describe
  their results from data they already hold.

## Alternatives considered

- Keep the table and add a row per new tool: the coupling the audit and ADR-0057 removed.
- A generic "diff by re-reading the file" in the host: the host would guess what a tool did.

## Evidence

Pending: the gate and CI of the merge of task/describe-results, the per-tool tests, the host
test with a renamed face and an unknown tool, and the SLAB oracle unchanged; filled at
acceptance.
