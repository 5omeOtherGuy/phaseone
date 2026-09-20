# Environment assembly and host — specification

This is where the harness reshapes itself around the model (pillar 2). One resolved
selection drives BOTH the prompt and the exact tool set; the core does none of it.

## Crates

| Crate | Owns |
|---|---|
| `p1-assembly` | Environment files → validated `ResolvedEnvironment` → `AgentParts`. Knows the contracts and a *factory catalog* it is handed; names no concrete provider or tool. |
| `p1-host` (binary `p1`) | The composition root: the ONE place that names concrete crates. Builds the catalog, loads environments, headless run + plain terminal prompt, event rendering, authorization policy, journal store. |

## Environment files

An environment is a directory, shipped in `environments/<name>/` and overridable from
`$P1_CONFIG_DIR/environments/<name>/` (default `~/.config/p1`):

```
environments/claude/environment.toml
environments/claude/prompt.md
environments/gpt/environment.toml
environments/gpt/prompt.md
```

```toml
# environments/claude/environment.toml
family   = "claude"
provider = "anthropic-subscription"        # catalog key
model    = "claude-sonnet-5"
[options]
reasoning_effort = "medium"                # optional; absent = adapter default

[[tools]]
module = "read"                            # catalog key
[[tools]]
module = "edit"
[[tools]]
module = "write"
[[tools]]
module = "grep"
[[tools]]
module = "shell"
# optional per tool:  name = "…"  description_file = "…"  variant = "…"   → ToolFace override
```
`environments/gpt/environment.toml`: provider `openai-codex-subscription`, model
`gpt-5.6-sol`, tools `shell` and `apply_patch` — the Codex-native pair, nothing else.

`prompt.md` is a WHOLE prompt file per family. The only substitutions, `{{…}}`:
`{{workspace}}`, `{{date}}`, `{{os}}`, `{{tool_names}}` (comma-separated, in order),
and `{{tool:<module>}}` → that tool's assembled model-facing NAME. An unknown placeholder,
or `{{tool:x}}` for a tool that is not in this environment, is an assembly error — a prompt
can never mention a tool by placeholder that the agent does not have. No fragment engine.

## Catalog (compile-time composition)

```rust
pub struct Catalog { /* name → constructor closures; built by the host */ }
impl Catalog {
    pub fn provider(&mut self, key: &str, make: ProviderFactory);   // Fn(&ProviderSpec) -> Result<Arc<dyn Provider>, AssemblyError>
    pub fn tool(&mut self, key: &str, make: ToolFactory);           // Fn(&ToolSpec, &ToolServices) -> Result<Arc<dyn Tool>, AssemblyError>
}
```
`ToolServices` carries what tools of ONE agent share: workspace root and that agent's
`ObservedFiles` — created fresh per agent, so two agents never share read-state. The catalog
is not shown to agents; configuration cannot name a module that was not compiled in.

## Assembly

`assemble(catalog, environment, overrides) -> Result<(ResolvedEnvironment, AgentParts-minus-host-policies), AssemblyError>`

Fails BEFORE a run starts, each with its own `AssemblyError` variant and a message naming
the file and key:
1. unknown provider key / unknown tool module key;
2. duplicate model-facing tool names;
3. a declaration kind the provider cannot carry (`Freeform` on a function-only route) — via `Provider::validate`;
4. an explicit option the route cannot carry — via `Provider::validate`;
5. unknown or unsatisfied prompt placeholder; missing `prompt.md`;
6. family/provider mismatch is NOT guessed around: a `family = "gpt"` environment naming an
   Anthropic provider assembles only if every declaration and option validates — and there is
   no silent fallback to a generic preset. A `generic` environment exists only if selected by name.
7. empty tool list is allowed (a pure chat agent) — not an error.

`ResolvedEnvironment` (serialisable, journalled, printable with `p1 env show <name>`):
environment name, `RouteDescription`, effective prompt text, each tool's
`(declaration, identity)`, options. NEVER credentials, token paths' contents, or env vars.

Prompt/tool coherence tests (both shipped environments): every tool name appearing in
backticks in `prompt.md` after substitution is an assembled tool; every assembled tool is
mentioned at least once; the GPT prompt does not mention `edit`/`write`, the Claude prompt does
not mention `apply_patch`.

## Host

```
p1 [--env claude|gpt|<name>] [--workspace DIR] [--session FILE] [--resume] [--yes] "prompt"    # headless, one turn… until done
p1 [--env …]                                                                                  # plain terminal prompt loop
p1 env show <name>                                                                            # resolved environment, no secrets
```
- Interactive: while waiting at the prompt the host also waits on the agent's inbox; a
  notification (a worker finishing) runs inbox turns at once, then the prompt is shown again.
  The line source is cancel-safe, so a half-typed line survives that.
- Headless: runs the turn; while the agent has pending inbox messages it runs inbox turns;
  exits 0 on `Completed`, 1 on provider/commit/context failure, 130 on cancel (Ctrl-C cancels
  the turn's token; a second Ctrl-C exits).
- Rendering: plain line-oriented text to stdout (assistant text streamed; one line per tool
  start/finish with status; reasoning dimmed only if stdout is a TTY). Diagnostics to stderr.
- After every response and at exit: `model <route>/<model> · in <n|?> (cached <n|?>) · out <n|?> · cost <amount|unknown>`.
  Unknown is printed as `?`/`unknown`, never as 0.
- Authorization policy: `--yes` permits everything. Without it, headless mode permits
  `ReadOnly` and denies the rest with reason
  `Not permitted in headless mode without --yes.`; the interactive prompt asks
  `allow <tool> <one-line summary>? [y]es / [n]o / [a]lways for this tool` on the terminal.
- Journal: memory by default; `--session FILE` uses the JSONL store.
- No TUI, no colours beyond dim, no config beyond the environment files.
