# Kits

One kit per model family: how an agent on that family works (ADR-0217). A kit is the part of
an environment that is not the wire. It is catalog content, not a pack root: `kits/` is a
catalog sibling like `routes/`, so a kit lives at `<root>/kits/<family>/`, here in the root
catalog for the shipped ones, under `<pack>/kits/` in a pack and under `~/.config/p1/kits/` in
the user directory; the first file per relative path wins whole (ADR-0201). Giving a family
directory alone to `P1_PACKS` installs nothing.

`kit.toml` holds every table an environment carries today except `route`, `profile`, `account`
and `[[mcp_servers]]`, which stay environment-only:

- `[[tools]]`: the tool set, one table (the Codex CLI's set under Codex's faces for GPT,
  ADR-0219; the `read`/`edit`/`write`/`grep` set for Claude);
- the prompt: until the prompt profiles of `docs/design/prompts.md` land (#779), a whole
  prompt file the kit names with `prompt_file`, relative to the file that names it; then
  `prompt`, the profile name;
- `[context]` and `summarize.md`: the compaction preferences, a `policy` key naming the
  context-policy module (absent: the summarizing one, `p1/context/summarizing`, the only one
  the host composes today) and the summarizer override beside the file whose `[context]` it is.
  The window is wire data: a `[context]` without `window_tokens` takes the profile's
  `context_tokens` lowered by the route binding's `context_limit`, and refuses to start when
  neither states one; a table that states `window_tokens` keeps it, narrowed by both;
- `[tool_concurrency]`, `[options]`, `[instructions]`, `[skills]`, `[capabilities]`.

`<profile-id>.toml` next to `kit.toml` overrides tables for one model (the profile id, never a
wire id). `base/` is the layer every kit falls back to. For each table the resolution is the
model's file, else the family's `kit.toml`, else `base/kit.toml`, each found across the layers,
specificity before layer priority; a table is taken whole, never merged, and the keys a
replacing table leaves out take their defaults (`[tool_concurrency]` without `shell_reads`
means `shell_reads = true`). To tune one table, copy the whole kit file into
`~/.config/p1/kits/<family>/`, as one copies a whole environment today. An environment's own
tables replace the kit's the same way; a role environment (a reviewer, a finder) keeps its own
tools and prompt that way, and an empty table (`[instructions]` with no keys) keeps a table on its
defaults where the kit would set it. An environment's own `prompt.md` replaces the kit's prompt.
`description_file` and `summarize.md` resolve relative to the file that names them.
`p1 env show` prints a `table` line per table naming the file it came from, and a `context` line
with the effective window and thresholds. A `--model`, `/model` or worker model of another
family takes that family's kit.

The family is the `family` key of the model profile in `profiles/`; a family without a kit uses
`base`. The family and model prompt variants (`<id>.<family>.md`, `<id>.<model>.md`) move here
under `<family>/prompts/` from `environments/prompts/fragments/` once #779 has landed; the role
profiles (`main`, `fable`, `manager`) stay shared under `environments/prompts/`.

| Kit | Family key | Shipped profiles | Today's hand-made environments it replaces |
|---|---|---|---|
| `base` | | | the part every environment repeats |
| `claude` | `claude` | claude-sonnet-5, claude-opus-5-5, claude-opus-5, claude-fable-5, claude-sonnet-4-6, claude-opus-4-6 | `claude`, `claude2` |
| `gpt` | `gpt` | gpt-6-sol, gpt-6-luna, gpt-6-astra, gpt-5.6-sol, gpt-5.6-luna, gpt-5.6-terra, gpt-5.5 | `gpt` |
| `glm` | `glm` | glm-5.3, glm-5.3-flash, and the `cline` connector's glm-5.3-flash-clinepass | `glm`, `glm-go`, `glm-messages` |
| `deepseek` | `deepseek` | deepseek-v4.1-flash | `deepseek`, `deepseek-messages`, `cline` and their aliases; `deepseek-review` stays an environment over this kit |
| `kimi` | `kimi` | kimi-k3 | `kimi` |

The free-tier families the shipped profiles also declare (`space-bunny`, `mimo`, `muse-free`)
use `base` until someone writes them a kit.

Status: unit 4 of #924 built kits in `p1-assembly` (`crates/p1-assembly/src/kit.rs`) and filled
them from the environments of b98a8167; `environments/default` and the owner pack's environments
are thin. The installer that accepts the `kits/` share member shipped first (#926). The
family and model prompt variants move here once #779 lands.
