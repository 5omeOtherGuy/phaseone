# System prompts

Every agent and subagent system prompt p1 ships lives in this directory (the few prompt texts
that live elsewhere are listed below). An environment is a directory
`<name>/` holding `environment.toml` (route, model profile, tools, options) and `prompt.md`
(the whole system prompt for that environment). Specification: `docs/design/assembly.md`;
seam row: "System prompt assembly" and "Per-agent persona" in `docs/design/seams.md`.

## Which prompt each agent gets

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

## Template syntax

`prompt.md` is plain Markdown with these substitutions and nothing else
(`crates/p1-assembly/src/lib.rs`, module docs):

- `{{workspace}}`, `{{date}}`, `{{os}}`, `{{scratch}}`
- `{{tool_names}}`: the agent's tool names, comma-separated
- `{{tool:<module>}}`: the model-facing name of that tool module
- `{{#tool:<module>}}` … `{{/tool:<module>}}`: a section kept only when the agent has that
  tool, so one prompt serves a lead and a worker granted a subset

An unknown placeholder, or `{{tool:x}}` outside a section for a tool the environment lacks, fails
assembly. There is no escape for a literal `{{`.

After rendering, the host appends standing instruction files (`[instructions]` in
`environment.toml`, default `AGENTS.md`) and the skill index (`[skills]`); see ADR-0151.

## Prompt text that lives elsewhere

| Text | Where | Override |
|---|---|---|
| Context summarizer prompt | `DEFAULT_SUMMARIZER_PROMPT` in `crates/p1-context/src/lib.rs`, mirrored in `crates/p1-host/src/summary.rs` (a test holds them equal) | `<environment>/summarize.md` |
| Tool descriptions | each tool's crate (`crates/p1-tool-*/`) | `description_file = "…"` on a `[[tools]]` entry, relative to the environment directory |
| Audit prompts | `scripts/audits/environments/*/prompt.md` | used only by the audit scripts |

## Tweaking a prompt

Try a change locally without committing it: copy the whole environment directory into the
override directory, which wins over the shipped one by name.

```
mkdir -p ~/.config/p1/environments
cp -r environments/claude ~/.config/p1/environments/claude
$EDITOR ~/.config/p1/environments/claude/prompt.md
p1 env show claude        # source lines, then JSON with the rendered system_prompt and tools;
                          # fails on a bad placeholder
```

To preview a checkout's own prompts instead of the installed ones, point the shipped
directory at it: `P1_ENVIRONMENTS_DIR=$PWD/environments p1 env show claude`.

The output's `source  environment` line names the copy and the shipped file it shadows;
routes and profiles still come from the shipped tree. `$P1_CONFIG_DIR` moves the override
directory (default `~/.config/p1`, which p1 requires to be mode 0700); a copied environment
stops following later changes to the shipped one, so delete the copy when done. A complete
`subagents.toml` in the override directory replaces the shipped one the same way.

To ship a change, edit the file here and run `cargo test -p p1-assembly`:
`tests/lead_prompt_coherence.rs` renders every shipped prompt for every tool subset and
checks that it names only tools the agent has, and every tool it has
(`tests/prompt_sections.rs` covers the template syntax itself). A pull request whose changed
files are all Markdown takes the gate's docs-only fast path and skips these tests in CI, so
run them locally before a prompt-only push.
