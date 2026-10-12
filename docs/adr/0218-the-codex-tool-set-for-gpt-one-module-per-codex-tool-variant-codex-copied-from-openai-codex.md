---
adr: 218
title: The Codex tool set for gpt: one module per Codex tool, variant codex, copied from openai/codex
status: accepted
date: 2026-10-13
deciders: owner
supersedes: []
superseded_by: []
sources: ["#933", "openai/codex rust-v0.162.1 (092d3acd)"]
---
# ADR-0218: The Codex tool set for gpt: one module per Codex tool, variant codex, copied from openai/codex

## Context

Owner, 2026-10-13: "I want to copy every codex native tool (that can be used within p1) exactly, so we can have a starting tool set for gpt that matches the native harness exactly." A GPT model on the Codex subscription was tuned on the Codex CLI's tools; p1's own tools carry other names, schemas and result text, and the gpt kit (ADR-0217) so far lists p1's tools under p1's faces.

What the Codex CLI gives a model is per-model data, not code (`codex-rs/models-manager/models.json` at `rust-v0.162.1`). Two shapes exist. The **Direct set**: `exec_command` and `write_stdin` (a PTY session, `shell` is gone), `apply_patch` as a freeform tool with a Lark grammar, `view_image`, `request_user_input` (Plan mode only), hosted `web_search`, goals, `image_gen.imagegen`, `tool_search` with the deferred multi-agent tools, MCP resource tools; `update_plan` exists and is off by default. **Code mode** (every gpt-6 and gpt-5.6 model): the model sees a JavaScript `exec` tool run in a V8 isolate plus `wait`, `request_user_input`, `request_user_input_async`, `clock.sleep` and a `collaboration` namespace, and every Direct tool is nested as `tools.<name>()` inside `exec`; the tools travel as one `functions` namespace in an `additional_tools` developer item.

p1 cannot run code mode: a JavaScript isolate that orchestrates tools is another harness, not a tool, and p1's wire has no namespace or hosted tool kinds. p1 can carry the Direct set over the capabilities its own tools use. A kit entry can rename a module and replace its description (`[[tools]] name`, `description_file`), but the parameter schema and the result text are the module's own, so an exact copy is a module.

## Decision

1. **One module per Codex tool, `variant = "codex"`.** Each is its own package with Codex's name, parameter schema, description and result text byte for byte, over the capability the matching p1 tool already uses; the semantics are p1's (package.md: "two packages may ship the same tool under different variants"). No existing module changes.

   | Codex tool | package, key | over |
   |---|---|---|
   | `exec_command` | `p1/exec-command`, `exec_command` | the shell's host composition (`shell_entry`, now keyed): sandbox, jobs, output store |
   | `write_stdin` | `p1/write-stdin`, `write_stdin` | `process-jobs`, `tool-outputs` |
   | `apply_patch` | `p1/patch-codex`, `codex_apply_patch`, declared as `apply_patch` by the module itself (a locked host entry takes no `name` override) | `p1-tool-patch-logic` |
   | `request_user_input` | `p1/request-user-input`, `request_user_input` | `user-questions` (ADR-0116) |
   | `update_plan` | `p1/update-plan`, `update_plan` | the session plan (ADR-0159), which reads the plan from the call's journalled arguments |

   The faces live in pure guest crates (`p1-exec-command-guest`, `p1-patch-codex-guest`, `p1-request-user-input-guest`, `p1-update-plan-guest`), each pinned by tests against the Codex source strings.

2. **What p1 cannot do, the face says plainly** (`docs/design/tools.md`): `exec_command` runs `cmd` with `bash -lc` for `yield_time_ms`, after which the host hands the command over to a session job (ADR-0123) and the response names it as the session; `tty`, `shell` and `login = false` are accepted and ignored; a `require_escalated` request gets Codex's own refusal for an approval policy that never asks (#761 is the escalation seam). `write_stdin` polls a job; a p1 job has no stdin, so a non-empty write fails with Codex's `stdin is closed` sentence. `request_user_input` keeps p1's invitation rule and reports it with Codex's `unavailable in Default mode` sentence. A sandboxed run still appends p1's sandbox paragraph to `exec_command` (ADR-0035).

3. **Out, as named gaps:** `view_image` (a tool result is text; images need the attachment seam on the tool side, #710), hosted `web_search` and `image_gen.imagegen` (provider wire, #717), goals, `tool_search` and the multi-agent tools (a namespace wire kind; p1's workers stay), MCP resource tools, `clock.*`, the async user tools, `request_permissions`, code mode.

4. **The gpt kit lists Codex's default Direct set that p1 carries** (after unit 4 of #924 lands): `exec_command`, `write_stdin`, `apply_patch` (the Codex face), `request_user_input`; `update_plan` is shipped and, as in Codex, not listed. The p1-only tools leave the gpt kit unless the owner keeps them.

5. **Attribution.** Codex is Apache-2.0; the copied strings keep its notice in `docs/licenses/openai-codex/`, and each face crate names the files it copies and the tag.

## Consequences

- A GPT model on p1 reads the tool declarations it was tuned on. The behaviour behind them is p1's, and the gaps above are documented, not hidden.
- `shell_entry` is registered under two keys; `locked_module_services` links jobs and questions to the new packages by name; `HOST_ENTRIES` grows by five.
- `p1-todo-session` keeps the arguments of the latest assistant item's tool calls so an `update_plan` call can publish a snapshot while the model reads `Plan updated`.
- Copying is per Codex release: a later Codex changes a face, p1 follows by editing the constants and the pinned tests, naming the new tag.

## Alternatives considered

- **Kit-level rename and description only** (`name`, `description_file`): the schemas and result text would still be p1's, which is the part the model was tuned on. Rejected.
- **Change p1's own `apply_patch`, `shell`, `ask_user_question`, `todo_write` to Codex's faces:** every Claude-family environment would get Codex's wording and p1's function-form `apply_patch` would carry a description that says it is freeform. Rejected; a variant per family is what the package manifest's `variant` is for.
- **Code mode:** a JavaScript runtime in a tool module, or a host-side isolate, to match what gpt-6 models see natively. Out of scope; recorded as a gap.
- **Per-family descriptions as prompt fragments (#780):** complementary, not a replacement; it covers wording, not schemas.

## Evidence

- Owner words of 2026-10-13 quoted in Context; #933.
- The Codex CLI read at tag `rust-v0.162.1` (commit 092d3acd) on 2026-10-13:
  `codex-rs/models-manager/models.json` (`gpt-6.1-sol`: `tool_mode: code_mode_only`,
  `use_responses_lite: true`, `apply_patch_tool_type: freeform`, `multi_agent_version: v2`;
  `gpt-5.5`: no `tool_mode`), `core/src/tools/spec_plan.rs` (the assembly: `add_shell_tools`,
  `add_core_utility_tools`, `update_plan` behind `config.update_plan_enabled`),
  `core/src/tools/handlers/{shell_spec,apply_patch_spec,plan_spec,request_user_input_spec}.rs`,
  `core/src/tools/context.rs` (`ExecCommandToolOutput::response_text`),
  `core/src/unified_exec/{mod,errors}.rs`, `utils/string/src/truncate.rs`,
  `utils/output-truncation/src/lib.rs`, `apply-patch/src/lib.rs` (`print_summary`),
  `core/src/tools/mod.rs` (`format_exec_output_for_model`), `LICENSE` (Apache-2.0).
- Facts checked at 03503572 on 2026-10-13: a `[[tools]]` entry carries `module`, `name`,
  `description_file`, `variant` and nothing else (`crates/p1-assembly/src/lib.rs`
  `ToolToml`); a tool's schema is its declaration's (`p1_contracts::tool::DeclarationKind`);
  `shell_entry` in `crates/p1-host/src/catalog/tools.rs` composes the process capability with
  the sandbox, the job registry and the output store for the `shell` key alone;
  `locked_module_services` links `process-jobs` and `user-questions` by package name, and
  the loader's manifest check (`crates/p1-module-runtime/src/loader.rs`) refused the
  `process-jobs` grant outside `p1/shell` and `p1/shell-job` and the `user-questions` grant
  outside `p1/ask-user-question`, so both allow lists widen to the Codex packages;
  `p1-todo-session` publishes the plan from a committed `p1/todo` result; a tool outcome is
  text (`ToolOutcome.content`); a foreground command that reaches its deadline is adopted
  into a session job when a registry is attached (`crates/p1-module-runtime/src/process/capability.rs`).
