# System prompts

The system prompt p1 ships lives in this directory (the few prompt texts that live elsewhere
are listed below). An environment is a directory `<name>/` holding `environment.toml` (route,
model profile, tools, options) and `prompt.md` (the whole system prompt for that environment).
Specification: `docs/design/assembly.md`; seam row: "System prompt assembly" and "Per-agent
persona" in `docs/design/seams.md`.

## Which prompt each agent gets

The core catalog ships one environment (ADR-0197):

| Environment | Prompt | Route / profile | Notes |
|---|---|---|---|
| `default` | `default/prompt.md` | anthropic / claude-sonnet-5 | the API-key Messages route; what a bare `p1` runs |

More environments come from packs layered over this directory (`packs` in
`~/.config/p1/settings.toml` or `P1_PACKS`, ADR-0201). The owner's environments, their
subagents and their prompts are the owner pack's: `packs/owner/README.md`.

A subagent configuration (`subagents.toml`) sits beside a pack's environments. A worker started
through the delegation tools with an explicit `system_prompt` gets that text instead, in full.

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

## Prompt profiles (V1, replacing the whole files)

`prompts/` holds the V1 system prompts as fragments (`prompts/fragments/<id>[.<family>|.<model-profile>].md`)
that the assembler composes per profile (`prompts/<profile>.toml`), family and model:
specification `docs/design/prompts.md`, authoring rules `prompts/README.md`. Until the
assembler and the migration land, the environments above keep their `prompt.md` and nothing
under `prompts/` is read at run time.

## Prompt text that lives elsewhere

| Text | Where | Override |
|---|---|---|
| Context summarizer prompt | `DEFAULT_SUMMARIZER_PROMPT` in `crates/p1-context/src/lib.rs`, mirrored in `crates/p1-host/src/summary.rs` (a test holds them equal) | `<environment>/summarize.md` |
| Tool descriptions | each tool's crate (`crates/p1-tool-*/`) | `description_file = "…"` on a `[[tools]]` entry, relative to the environment directory |
| Audit prompts | `packs/owner/scripts/audits/environments/*/prompt.md` | used only by the audit scripts |

## Tweaking a prompt

Try a change locally without committing it: copy the whole environment directory into the
override directory, which wins over the shipped one by name.

```
mkdir -p ~/.config/p1/environments
cp -r environments/default ~/.config/p1/environments/default
$EDITOR ~/.config/p1/environments/default/prompt.md
p1 env show default       # source lines, then JSON with the rendered system_prompt and tools;
                          # fails on a bad placeholder
```

To preview a checkout's own prompts instead of the installed ones, run a debug build of the
checkout, which reads the shipped catalog from the roots of `catalog-roots.txt`, and add the
owner pack as a pack layer for the owner's environments:
`P1_PACKS=$PWD/packs/owner target/debug/p1 env show claude`.

The output's `source  environment` line names the copy and the shipped file it shadows;
routes and profiles still come from the shipped tree. `$P1_CONFIG_DIR` moves the override
directory (default `~/.config/p1`, which p1 requires to be mode 0700); a copied environment
stops following later changes to the shipped one, so delete the copy when done. A complete
`subagents.toml` in the override directory replaces the shipped one the same way.

To ship a change, edit the file here and run `cargo test -p p1-assembly`:
`tests/lead_prompt_coherence.rs` renders every shipped prompt, and the owner pack's, for every
tool subset and checks that it names only tools the agent has, and every tool it has
(`tests/prompt_sections.rs` covers the template syntax itself). A pull request whose changed
files are all Markdown takes the gate's docs-only fast path and skips these tests in CI, so
run them locally before a prompt-only push.
