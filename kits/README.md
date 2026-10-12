# Kits

One kit per model family: how an agent on that family works (ADR-0217). A kit is the part of
an environment that is not the wire. It is catalog content, not a pack root: `kits/` is a
catalog sibling like `routes/`, so a kit lives at `<root>/kits/<family>/`, here in the root
catalog for the shipped ones, under `<pack>/kits/` in a pack and under `~/.config/p1/kits/` in
the user directory; the first file per relative path wins whole (ADR-0201). Giving a family
directory alone to `P1_PACKS` installs nothing.

`kit.toml` holds every table an environment carries today except `route`, `profile`, `account`
and `[[mcp_servers]]`, which stay environment-only:

- `[[tools]]`: the tool set, one table (the Codex pair `shell` + `apply_patch` for GPT, the
  `read`/`edit`/`write`/`grep` set for Claude);
- `prompt`: the prompt profile of `docs/design/prompts.md`;
- `[context]` and `summarize.md`: the compaction preferences, a `policy` key naming the
  context-policy module (the summarizing one by default) and the summarizer override; the
  served window and output limits are wire data (the profile and the route's model binding);
- `[tool_concurrency]`, `[options]`, `[instructions]`, `[skills]`, `[capabilities]`.

`<profile-id>.toml` next to `kit.toml` overrides tables for one model (the profile id, never a
wire id). `base/` is the layer every kit falls back to. For each table the resolution is the
model's file, else the family's `kit.toml`, else `base/kit.toml`, each found across the layers,
specificity before layer priority; a table is taken whole, never merged, and the keys a
replacing table leaves out take their defaults (`[tool_concurrency]` without `shell_reads`
means `shell_reads = true`). To tune one table, copy the whole kit file into
`~/.config/p1/kits/<family>/`, as one copies a whole environment today. An environment's own
tables replace the kit's the same way; a role environment (a reviewer, a finder) keeps its own
tools and prompt that way. `description_file` and `summarize.md` resolve relative to the file
that names them.

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

Status: these directories are the layout of ADR-0217. Unit 4 of #924 builds kits in
p1-assembly and fills them from today's environments, over two releases (the installer learns
the `kits/` share member first). Until then p1 reads nothing here.
