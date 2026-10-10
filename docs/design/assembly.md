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

An optional `[tool_concurrency]` table (ADR-0118, owner amendment 2026-10-09) says how the
tool calls of one response execute: `max_parallel` (integer 1 to 10, default 10; 1 runs every
call alone, as before ADR-0118) and `shell_reads` (bool, default `true`; `false` runs every
`shell` call alone whatever its read-only classifier says). An absent table or key takes the
default; a value out of range is `AssemblyError::InvalidToolConcurrency` naming the key, and
a wrong type or an unknown key is an environment-file error naming it. The validated values
are `EnvironmentFile::tool_concurrency`, `ResolvedEnvironment::tool_concurrency` (printed by
`p1 env show`) and `ToolServices::tool_concurrency`; the host hands `max_parallel` to the core
and `shell_reads` to the shell entry. Only `environments/claude` sets it (`shell_reads =
false`; `claude2` inherits it as an alias).

`prompt.md` is a WHOLE prompt file per family. The only substitutions, `{{…}}`:
`{{workspace}}`, `{{date}}`, `{{os}}`, `{{tool_names}}` (comma-separated, in order),
and `{{tool:<module>}}` → that tool's assembled model-facing NAME. An unknown placeholder,
or `{{tool:x}}` for a tool that is not in this environment, is an assembly error — a prompt
can never mention a tool by placeholder that the agent does not have. No fragment engine.

### Configuration reads

`environment.toml`, `prompt.md`, optional `summarize.md`, tool `description_file`
(including absolute paths), profiles and `modules.lock` use one protected reader.
These are operator configuration from config/install directories, not workspace files.
Each input must be a regular UTF-8 file of at most 1 MiB. The reader enforces this
byte cap both on opened-file metadata and while reading, so sparse files and growth
cannot bypass it; nonblocking opens reject FIFOs without waiting for a writer.
Ordinary symlinks and hard links remain supported. Credential paths and opened-file
identities are refused using the file tools' credential policy before any bytes are
read, with diagnostics that never include refused contents. Missing optional files
still mean no override; other read failures stop assembly.

`ModulesLock::parse` also enforces the 1 MiB text cap and at most 1,024 entries per
lock. Worlds must exactly name one of the six supported module classes at `1.0.0`:
`tool`, `provider`, `context-policy`, `authorization-policy`,
`workflow-implementation` or `workflow-decision`, under `p1:module/`. Malformed or
unsupported worlds are rejected even in entries the environment does not select.
Release identity comparison remains the host's responsibility.

## Catalog (composition root)

ADR-0071 (owner 2026-09-25): the catalog's entries become WebAssembly modules the host loads by
name under the same keys; until that migration lands they are the compile-time constructors below.

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

### Standing instruction files and skills (ADR-0151)

Assembly appends instruction data after prompt substitution (no placeholders are
expanded inside these files). Optional `[instructions]` defaults to `enabled =
true`, `files = ["AGENTS.md"]`, `max_bytes = 32768` (1..1048576). Global file
comes first: settings.toml `instructions_global`, default `~/.agents/AGENTS.md`.
Then first existing filename per directory from git root to workspace; without
git only workspace is searched. The UTF-8 text total is capped: truncate crossing
file, drop later files and name affected paths in one visible notice. Missing
global is silent; unreadable files warn and do not prevent assembly.

Optional `[skills]` defaults to roots `~/.agents/skills`, `.agents/skills` and
`max_listing_chars = 8000` (100..100000). Relative roots are searched along the
same project walk. User roots precede project roots, retaining configured order;
first valid skill name wins. One-level SKILL.md discovery parses YAML name and
required description, warns/skips malformed data, and freezes bodies at assembly.
Discovery is `p1-skill-fs`'s responsibility; `p1-tool-skill` renders the listing
and provides the tool over `p1-contracts::skill::SkillSource`. Host supplies this
composition to the catalog without coupling assembly to either implementation.
Only an environment selecting module `skill` gets discovery and the appended
XML listing. Over the listing budget all names remain, descriptions are omitted.
The model-facing tool name is taken from assembly, including face overrides.

Claude selects CLAUDE.md then AGENTS.md, with claude and agents skill roots; GPT
selects AGENTS.override.md then AGENTS.md. GLM/Kimi/DeepSeek select AGENTS.md and
skill; Zen disables automatic instructions and offers no skill. Aliases inherit.
`env show` prints settings, paths, sizes, short loaded-text/body hashes and
warnings; the journal's AssemblyIdentity records instruction text hashes and
skill names/paths with default-empty fields for old journals. These automatic
sections apply to each assembled agent, unlike legacy opt-in CLI additions.

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
- Authorization policy: FULL ACCESS IS THE DEFAULT (owner decision 2026-09-20, ADR-0038): every
  tool call is permitted, headless and interactive, without any flag — development must never
  be blocked by a permission question nobody is there to answer. `--ask` opts into the
  restrictive policy: headless permits `ReadOnly` and denies the rest with reason
  `Not permitted in headless mode with --ask.`; the interactive prompt asks
  `allow <tool> <one-line summary>? [y]es / [n]o / [a]lways for this tool` on the terminal.
  `--yes` is still accepted and means the default (so existing scripts keep working);
  `--yes` together with `--ask` is a usage error. Confinement of the FILE tools to the
  workspace is not a permission and stays always on; the shell sandbox stays opt-in.
- Journal: memory by default; `--session FILE` uses the JSONL store.
- No TUI, no colours beyond dim, no config beyond the environment files.

Composition seam: the host drives an agent through a `FrontEnd`
(`crates/p1-host/src/frontend.rs`) — an ordinary value passed down, not a registry or a plugin.
It supplies the parent's event sink (the host still wraps it in its `ActivityTee`, and in a
headless run in the `StallWatcher`), the labelled sink for each delegated worker, the
authorization policy shared by the parent and every worker, and the run loop (`Agent` +
`CancellationToken` + the optional worker service + the host's headless stall guard → exit code,
then a one-shot `finish`). The default `LineFrontEnd` keeps the `HostPolicy` and line `Renderer`
and the existing headless/interactive drivers exactly as before, so line and delegation behaviour
is byte-identical. It is also the front end that declares `is_headless` (the CLI rule); a terminal
UI overrides that to `false`, so the §3c guard stays host policy and is never installed for it.
`run.rs` constructs the line front end at one branch point; a session that owns its own terminal
UI implements the same trait and calls `run_with_front_end` instead.
