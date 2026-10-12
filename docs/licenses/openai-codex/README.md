# openai/codex — Apache-2.0 attribution

Parts of p1 copy model-facing text and small functions from the Codex CLI
(https://github.com/openai/codex), Copyright OpenAI, licensed under the Apache License,
Version 2.0 (`LICENSE` in this directory). p1 itself is MIT; the copied parts stay under
Apache-2.0 as that license requires, and this file is the notice.

Copied at tag `rust-v0.162.1` (commit 092d3acd6bec3e3a14bdc7e7a2810ab628ab759d), decision
ADR-0219:

| p1 crate | Codex source (`codex-rs/`) | What |
|---|---|---|
| `crates/p1-exec-command-guest` | `core/src/tools/handlers/shell_spec.rs`, `core/src/tools/handlers/unified_exec.rs`, `core/src/tools/context.rs`, `core/src/unified_exec/mod.rs`, `core/src/unified_exec/errors.rs`, `utils/string/src/truncate.rs`, `utils/output-truncation/src/lib.rs` | the `exec_command` and `write_stdin` schemas, descriptions, defaults, response text, error texts and middle truncation |
| `crates/p1-tool-patch/codex` | `core/src/tools/handlers/apply_patch_spec.rs`, `core/assets/tools/apply_patch.lark`, `apply-patch/src/lib.rs`, `core/src/tools/mod.rs`, `core/src/tools/handlers/apply_patch.rs` | the `apply_patch` description, grammar and result text |
| `crates/p1-request-user-input-guest` | `core/src/tools/handlers/request_user_input_spec.rs`, `core/src/tools/handlers/request_user_input.rs`, `protocol/src/request_user_input.rs` | the `request_user_input` schema, description, answer JSON and sentences |
| `crates/p1-update-plan-guest` | `core/src/tools/handlers/plan_spec.rs`, `core/src/tools/handlers/plan.rs` | the `update_plan` schema, description and result text |

p1's own `apply_patch` grammar (`crates/p1-tool-patch/logic`, earlier work) is the same
grammar; this notice covers it too.
