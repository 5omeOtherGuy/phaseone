# The owner pack

The owner's setup of p1 (#853), the third tier beside the core catalog at the repository root
and the subscription connectors (`connectors/subscriptions/`, ADR-0200):

- `environments/`: the owner's agents, subagents (`subagents.toml`) and their prompts;
- `accounts/`: the owner's accounts on the connector routes (ADR-0139);
- `scripts/`: the owner's tooling (dogfood runs, fan-out, run reports, usage audits, the
  modularity audit, PR review briefs) with its tests;
- `docs/`: the owner's records (status, decisions ledger, dogfood runs, research memos);
- `tests/test_pack.py`: the pack's own check.

The pack is layered over the shipped catalog (ADR-0201): select it with
`packs = ["<checkout>/packs/owner"]` in `~/.config/p1/settings.toml` or with `P1_PACKS`. What an
environment is, the template syntax and how to tweak a prompt: `environments/README.md` at the
repository root.

## Which prompt each agent gets

Prompts are relative to `environments/`.

Main agents (the session's lead, chosen with `--env` or `/model`):

| Environment | Prompt | Route / profile | Notes |
|---|---|---|---|
| `claude` | `claude/prompt.md` | anthropic-subscription / claude-sonnet-5 | `claude2` is an alias (same prompt, account `claude-2`) |
| `gpt` | `gpt/prompt.md` | openai-codex-subscription / gpt-5.6-sol | Codex tool pair (`shell`, `apply_patch`) |
| `zen` | `zen/prompt.md` | opencode-zen / space-bunny-free | the Claude prompt plus a persona line; aliases `zen2`, `zen3` |
| `kimi` | `kimi/prompt.md` | kimi-coding-subscription / kimi-k3 | model-tuned |
| `glm`, `glm-go`, `glm-messages` | `<name>/prompt.md` | GLM routes / glm-5.3 | three identical copies, model-tuned |
| `deepseek`, `deepseek-messages`, `cline` | `<name>/prompt.md` | opencode-go-messages, cline-pass / deepseek-v4.1-flash | three identical copies, model-tuned; aliases `deepseek1..3`, `cline2` |
| `deepseek-review` | `deepseek-review/prompt.md` | opencode-go-messages / deepseek-v4.1-flash | code reviewer |

The routes are the subscription connectors' (`connectors/subscriptions/routes/`); the accounts
the environments name are this pack's (`accounts/`).

An alias directory (`alias_of = "…"` in its `environment.toml`) has no `prompt.md`; edit the
target's prompt. Identical copies are separate files: change all of them together, or the
copies drift.

Subagents are configured in `subagents.toml` (the `subagent_type`, its tool grant, default
model and `prompt_file`):

| Subagent | Prompt | How it is built |
|---|---|---|
| `task` | `task/prompt.md` | `inherit_prompt = true`: the parent's unrendered `prompt.md` comes first, then this file's worker role, rendered against the worker's own tools (ADR-0150) |
| `finder` | `finder/prompt.md` | standalone |
| `librarian` | `librarian/prompt.md` | standalone |

A worker started through the delegation tools with an explicit `system_prompt` gets that text
instead, in full.

## Checking a change

`p1 env check --dir packs/owner` checks every environment of the pack, and
`P1_BIN=<p1> python3 packs/owner/tests/test_pack.py` runs the pack's pins (context tables,
efforts, tool lists, instructions and skills, subagent presets, prompt coherence) against a
built p1. `cargo test -p p1-assembly --test lead_prompt_coherence` renders every prompt here
for every subset of its tools, which the binary has no command for.

A pull request that changes only `packs/` takes the gate's pack path (ADR-0206): CI installs
the latest release and runs `p1 env check --dir packs/owner`, `tests/` and the script tests
against it, with no build.
