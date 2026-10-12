# The owner pack

The owner's setup of p1 (#853), a pack layered over the shipped catalog: the repository root
and the connectors under `connectors/<vendor>/` (ADR-0217):

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

The environments are thin (ADR-0217): a model, a route, an account and the tables they change;
the rest is the kit of the model's family in the repository's `kits/`. Prompts in `kits/` are the
shipped kits'; the others are relative to `environments/`.

Main agents (the session's lead, chosen with `--env` or `/model`):

| Environment | Prompt | Route / profile | Notes |
|---|---|---|---|
| `claude` | `claude/prompt.md` | anthropic-subscription / claude-sonnet-5 | the `claude` kit with its own prompt, instruction order and skill roots; `claude2` is an alias (account `claude-2`) |
| `gpt` | `kits/gpt/prompt.md` | openai-codex-subscription / gpt-5.6-sol | the `gpt` kit: the Codex CLI's tools (`exec_command`, `write_stdin`, `apply_patch`, `request_user_input`; ADR-0218) |
| `zen` | `zen/prompt.md` | opencode-zen / space-bunny-free | the Claude prompt plus a persona line, its own tables over `base`; aliases `zen2`, `zen3` |
| `kimi` | `kits/kimi/prompt.md` | kimi-coding-subscription / kimi-k3 | the `kimi` kit |
| `glm`, `glm-go`, `glm-messages` | `kits/glm/prompt.md` | GLM routes / glm-5.3 | the `glm` kit, three thin environments |
| `deepseek`, `deepseek-messages`, `cline` | `kits/deepseek/prompt.md` | opencode-go-messages, cline-pass / deepseek-v4.1-flash | the `deepseek` kit (`cline` without `todo_write` and `skill`); aliases `deepseek1..3`, `cline2` |
| `deepseek-review` | `deepseek-review/prompt.md` | opencode-go-messages / deepseek-v4.1-flash | code reviewer over the `deepseek` kit |

The routes are the connectors' (`connectors/<vendor>/routes/`); the accounts the environments
name are this pack's (`accounts/`).

An alias directory (`alias_of = "…"` in its `environment.toml`) has no `prompt.md`; edit the
target's prompt. A kit prompt is shared by every environment of its family that has no
`prompt.md`: change it in the kit, or give one environment its own copy.

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
