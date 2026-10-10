# p1 seam catalog: dsh capabilities mapped to p1 ports

The seam catalog of ADR-0153 (2026-10-10; reworked the same day after the owner's decisions of
~13:3x and the scope answers that followed). Replaces the 2026-09 "modules and seams" draft v1,
kept verbatim as `seams-v1.md`: the section references in accepted records (ADR-0002, 0003, 0017,
0025, 0032 and others), `STATUS.md`, `docs/SLICE-REPORT.md`, `docs/design/design-summary.md` and
`scripts/check-core-isolation.sh` ("seams.md section 3/4/5/10/11", "§10") point to that file.
Evidence for every row: the dsh survey of 2026-10-10 at
`~/.agents/xo/dispatch/p1-lead-20261004/dsh-survey/SURVEY.md` and `family-<n>-*.md` in the
same directory (dsh `dsh-v0.2.0-rc.2`, commit 639ed01; p1 at b291a70a). This file copies no
evidence; it is the lookup table.

## How to use this

Before building or changing a feature, find its row. The row names the **port** p1 builds
(the contract) and the **adapters** behind it (the crates); build those, in that shape, even
when there is one implementation today (ADR-0153, Protected Variations). "Gap / next slice"
says which features dsh has and p1 does not, and which issue, if any, owns them. "Better than
dsh" is what p1 must not lose when the row changes. A feature with no row gets a row first,
through the lead. A row changes status in the pull request that changes the code. Nobody asks
"how does dsh do it?"; the answer is the row, and the survey holds the evidence.

**The dsh floor is about modularity, not features** (owner 2026-10-10, ADR-0153 Decision 2).
During the foundation phase, every seam p1 builds is at least as modular as dsh's equivalent in
dsh's default product (`dsh-base` and the packages it switches on; the surveyed tag
`dsh-v0.2.0-rc.2` is the reference). A feature p1 lacks is not below the floor. A feature p1
builds with a weaker seam than dsh's is: fewer ports, a source fused with its consumer, or a
composition that loses a variation point dsh protects. The "dsh counterpart" cell names dsh's
port package first, so the seams dsh separates can be read off the row; "Floor check" at the
end of this file says how to apply it.

Column key:

- **Port**: the capability, in the survey's words.
- **p1 contract (path)**: where the contract lives. `contracts/` means
  `crates/p1-contracts/src/`; `runtime/` means `crates/p1-module-runtime/src/`; `host/` means
  `crates/p1-host/src/`; `modules/wit/` is the WebAssembly interface. "host composition" means
  a trait in `p1-host` that is not yet a port (ADR-0153 Decision 1). "none" means no contract
  exists; "none by design" means the row needs no port and says why.
- **p1 adapters (crates)**: crates and `modules/` packages that implement the port today.
- **dsh counterpart**: the dsh packages, port package first.
- **Status**: `built` (the port exists and the capability's main path is there; the Gap cell
  lists what still differs in features), `partial` (part of it), `missing` (none of it),
  `different-by-design (decision)` (deliberately different, decision cited: an owner decision,
  an accepted ADR, or the owner's batch authorisation of 2026-10-10, written
  `owner 2026-10-10: lead batch, <reason>`), `in-flight #N` (being built, unmerged),
  `optional` (dsh ships it off by default or experimental; outside the floor). The owner answered
  the survey's 14 questions on 2026-10-10 (table at the end of this file): a row an answer touches
  carries `owner 2026-10-10: plan (seam: #N)` (its seam is designed under "Planned seams" and in
  issue N; no implementation is ordered), `owner: not now`, or `owner: after ACP door`; no row
  waits on an owner question. No status says whether a row is at the modularity floor; that is
  checked per row when it is built ("Floor check" below).
- **Gap / next slice**: which features dsh has and p1 does not, and the issue that owns them,
  if any. "none" means no feature gap. "(rank n)" points at the ranked feature-gap table.
- **Better than dsh**: what p1 already does better (the survey's `already:` items, shortened).

## dsh registries and p1's explicit composition

dsh selects most things through a registry on its plug-in context (`ctx.<name>`), filled by
rows in `cordis.patch.yml`. p1 forbids a service locator, global registry, auto-registration
and DI framework (ADR-0071, `AGENTS.md`). This table says where each dsh registry's job is done
in p1 instead; the family tables below say it per capability.

| dsh registry or service | What it selects | p1's explicit counterpart |
|---|---|---|
| Cordis `ctx` services, patch rows, bundles, profiles | which plug-ins run and with what config | `environments/<name>/environment.toml` (`[[tools]] module = ...`, `route`, `profile`, `[context]`, `[tool_concurrency]`, `[capabilities]`), resolved by `p1-assembly` against the host `Catalog` (compile-time map of names to constructors) and `modules.lock` (verified release packages, ADR-0071, ADR-0087); `p1-host` is the one root |
| `ctx.agents` (agent registry, `agent/*` waterfall) | live agents and step interception | one `Agent` built from an explicit `AgentParts` struct; observation through the `EventSink` trait; children through `p1-workers` with an injected agent factory |
| system-prompt section and variable registry; persona; presets | prompt text per agent | `environments/<name>/prompt.md` template with fixed placeholders (`{{workspace}}`, `{{tool:<module>}}`, conditional tool sections); presets are environment directories and `subagents.toml`; instruction and skill text appended by the host (#129) |
| `ctx.commands` | slash commands | a closed list built by the host; the ACP door publishes it with `available_commands_update` (#676) |
| `ctx.llm` adapter registry; `retryPolicy` per adapter | which wire adapter serves a request | the route file's `adapter` field, mapped by `p1-host` to a provider component; `retry_policy` presets in the route file (ADR-0137, ADR-0145); one provider per agent |
| DeepSeek API extensions registry | extra request fields | typed `[adapter_settings]` in the route file; no module can add a body field |
| `ctx.credentials`, `.env` layers | secrets by reference | `p1-auth` chain per route (env, p1 store, borrowed login; ADR-0040), `accounts/*.toml` (ADR-0139), pinned-directory files (ADR-0108), origin binding (ADR-0110) |
| `ctx.tools` registry and policy event waterfall | tools and per-call policy | the environment's `[[tools]]` list resolved to WebAssembly modules; one `AuthorizationPolicy::authorize` point (ADR-0024); masking as a `RedactingTool` wrapper at assembly; concurrency as a per-call tool property (ADR-0118) |
| `ctx.web` provider registry | search and fetch backends | none yet; planned (seam #717): #516 puts backend choice, credentials and network policy in the host behind a scoped `web` interface |
| MCP tool generations | externally served tools | none yet; planned (seam design on #695, family 7) |
| `ctx.jobs` | background jobs | `JobRegistry` per session in `p1-module-runtime` behind the `process-jobs` WIT interface (ADR-0117) |
| `ctx.subagents` provider table | child agent backends | one `WorkerService` implementation built at the root with an injected agent factory; `environments/subagents.toml` entries (ADR-0131); external-harness backends planned behind a `WorkerBackend` port (seam #715) |
| session store, projection registry, storage hub | durable state and derived views | one `CommitSink` built in `host/session.rs` (memory or JSONL, ADR-0021); one projection, `p1_core::resume::project`; each persistent file has one owner |
| telemetry backend slot | outbound capture | none; `EventSink` and `CommitSink` are explicit trait objects wired in `p1-host` |
| Typert registry, Remote, UI slots, client modules | typed RPC and browser extension points | none; ACP is the one wire (`p1-acp`, own versioned wire types, #670 D6) and front ends attach through the `FrontEndPort` in `contracts/frontend.rs` (ADR-0152) |
| `dsh-scope` | per-agent shadowing | ownership: `ToolServices` built per assembled agent, grants per worker (ADR-0050), capability snapshots on the tool (ADR-0103) |
| hook protocol listeners | user scripts in the loop | none yet; `AuthorizationPolicy` and `ContextPolicy` are the decision points; a hook core and a Claude Code bridge are planned (seam #711) |

## Family 1: kernel, composition and configuration (18 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Plug-in kernel: services, events, lifecycle cleanup | none by design: the kernel is `p1-core`; services are traits in `contracts/` passed into `AgentParts` | `p1-core`, `p1-assembly` (`Catalog`), `p1-host` (root) | `cordis` | different-by-design (ADR-0071, AGENTS.md) | none wanted: no dynamic lookup, no event bus | a missing dependency is a compile or assembly error, not a warning at settlement |
| Config-driven plug-in tree: rows, groups, includes | environment file schema (`EnvironmentToml`, `p1-assembly`) | `p1-assembly` (`load_environment`, `assemble`), `p1-host` (environment dirs, `modules.lock`) | `cordis-plugin-loader`, `cordis-plugin-group`, `cordis-plugin-include` | different-by-design (ADR-0071, ADR-0087) | none wanted: no row patch layers or runtime row edits | unknown keys rejected everywhere; cannot name a module outside the verified release |
| Product composition (which parts a product runs) | `environments/*/environment.toml` plus `settings.toml [capabilities]` (ADR-0124) | `p1-host` root; front ends over one assembly | `dsh-base` plus mode bundles (`dsh-acp-app`, `dsh-headless`, `dsh-sdk-app`, `dsh-sdk-minimal`, `dsh-web-app`) | different-by-design (modes are front ends on one assembly; #670) | no user override layer over a shipped environment (whole-file replace by name); decide if wanted | the prompt is tied to the tool set; assembly fails on a prompt naming a missing tool |
| Launcher and command line | none (host composition, `host/cli.rs`) | `p1-host` | `dsh`, `dsh-cmdline` | partial | no config-schema dump | one parser, no launcher/app split |
| Startup validation and failure policy | `AssemblyError` (`p1-assembly`) | `p1-assembly`, `p1-host` exit codes | `dsh-app-boot` | partial | no panic reporter, no saved startup diagnostics (rank 27) | fail-closed assembly: a half-built agent cannot run |
| Layered configuration and patches | strict serde structs for environments, routes, profiles, accounts; one bounded, credential-refusing reader | `p1-assembly`, `p1-host` | `dsh-app-boot` (patch layers), `schemastery` | partial | no override layers, no schema dump, no expression language (not a recorded decision) | no arbitrary code evaluated at boot (dsh evaluates `!!js`); unknown keys refused |
| Config editing and settings forms | none | none (credentials only: `p1 login`, ADR-0108) | `dsh-config-editor`, `dsh-settings` | missing | validated write path for non-credential config; ACP config options #675 | — |
| Plugin and bundle management, version compatibility | `modules.lock` and release manifest (`runtime/` loader, ADR-0087) | `p1-module-runtime`, `p1 modules` CLI, `scripts/install.sh`, `scripts/update.sh` | `dsh-plugin-manager`, `dsh-package-manifest`, `dsh-app-boot` | different-by-design (ADR-0065, ADR-0079, ADR-0087) | none wanted: the agent never changes what it is made of | digest checked before compile; no API loads caller-chosen bytes |
| Hot reload of code and configuration | `ShippedPolicy::reload` (host), ADR-0084, ADR-0078 | `p1-host` | `dsh-hmr`, `cordis-plugin-loader` | partial | reload of tools, providers, context policy; `/modules reload` from ACP (#676) | reload only between turns; a replaced policy cannot inherit consent |
| Launch environment and `.env` layering | `Locations` env lookup (injectable); credential chain ADR-0040 | `p1-auth`, process service (`env_clear`) | `dsh-launch-environment`, `dsh-app-boot` | different-by-design (owner 2026-10-10: lead batch, credentials come only through the `p1-auth` chain and children start from a cleared environment, ADR-0040) | none wanted: no `.env` layers | a project `.env` cannot reach credentials; children start from a cleared environment |
| Home directory and path helpers | none (`Locations` for credential paths only) | `p1-host` (`main.rs`, `routes.rs`) | `dsh-home-paths` | partial | one root helper: extend `Locations` to config and environment dirs | — |
| Outbound HTTP proxy policy | none | `p1-provider-http` (`ReqwestTransport`) | `dsh-http-proxy` | missing | proxy resolution, loopback bypass, diagnostics, child-process handling, egress test per call site (rank 15) | — |
| Per-agent and per-group scoping of contributions | `ToolServices` per assembled agent (ADR-0103); grants per worker (ADR-0050) | `p1-assembly`, `p1-workers` | `dsh-scope` | different-by-design (ADR-0050, ADR-0103) | none wanted | no shared registry to leak across agents |
| Runtime self-checks (invariants) | none (enforced by construction and in the gate) | `p1-core` pairing checks, `p1-provider-conformance`, `scripts/check-core-isolation.sh`, `scripts/check-module-boundaries.sh` | `dsh-invariants` | different-by-design (owner 2026-10-10: lead batch, invariants are enforced by construction and in the gate, not by a runtime registry) | none wanted | pairing and journal checks are on in every run (dsh ships them off) |
| Typed host-to-client RPC and runtime schema registry | ACP wire (`p1-acp`, own `wire::v1`, #670 D6); module protocol `p1-module-protocol` plus `modules/wit/` (ADR-0082) | `p1-acp` (#687, ADR-0154) | `dsh-typert-protocol`, `dsh-typert-registry`, `dsh-typert-loader` | in-flight #670 | no runtime schema registry (not a recorded decision; not needed today) | one fixed versioned wire and one module protocol; no generation step |
| Runtime-defined dual-half plug-ins (model-mounted packages) | none by design: the loader accepts no caller-chosen path or bytes (ADR-0071, ADR-0087) | — | `dsh-cordis-host-runner`, `dsh-cordis-client-runner` | different-by-design (ADR-0071, ADR-0087) | none wanted | no bash-level dynamic runner |
| Plug-in package inventory on provider requests | none | journal environment record (ADR-0080), `p1 env show` | `dsh-plugin-package-inventory-deepseek` | different-by-design (owner 2026-10-10: lead batch, a DeepSeek-platform diagnostic field; nothing about installed modules leaves the machine) | none wanted | nothing about installed modules leaves the machine |
| Disposal-aware timers | `contracts/clock.rs` (`Clock`) | engine tick clock (ADR-0112) | `cordis-plugin-timer` | different-by-design (owner 2026-10-10: lead batch, p1 has no plug-in lifetime to attach a timer to; the injectable `Clock` covers timing) | none wanted | injectable clock makes timing deterministic in tests |

## Family 2: utilities and primitives (11 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Atomic file replacement | `write_atomic` (`p1-workspace`, a library, not a port) | file tools (ADR-0111) | `dsh-atomic-write` | built | caller cannot set the new file's mode (credential writer covers that, ADR-0108) | fsyncs file and directory; exchange-based leaf replace; refuses multiply linked files |
| Cross-process writer lock and single-owner lease | `File::try_lock` (`p1-journal`), `lock_exclusive` (`p1-provider-http`) | — | `dsh-atomic-write` (`withFileLock`), `node-addon-system` | built | none | the lock dies with the process: no stale-lock takeover, no PID-reuse hole, no native addon |
| Timeouts, deadlines and stream idle watchdog | none shared: shell `timeout_seconds`, provider `first_byte`/`idle` (ADR-0069, ADR-0146), module tick deadlines (ADR-0112) | `p1-tool-shell`, `p1-provider-http`, `p1-module-runtime` | `dsh-timeout` | partial | no shared deadline type; idle-watchdog semantics of the provider stream unverified | — |
| Bounded output with an honest omission notice | `bound_output` (`p1-workspace`); shell head-and-tail `Capture` | tools; `read_output` paging (ADR-0109) | `dsh-output-retention` | partial | no shared retainer or notice formatter; `bound_output` is head-only (see rank 12) | full output stored, pageable and redacted before truncation |
| In-process FIFO queue | `std::collections::VecDeque` | — | `dsh-deque` | built | none | — |
| Shell-free native command runner and "open in desktop" | process service `Command` (`runtime/`), cleared environment | `p1-module-runtime` | `dsh-native-command` | missing, owner: after ACP door | open, reveal and default-application listing; client or host ownership undecided | child environment is cleared and rebuilt from an allow-list |
| Local sandbox launcher: Landlock fallback beside bubblewrap | none: the sandbox is a concrete `Sandbox` struct (`runtime/process/sandbox.rs`), not a port | bubblewrap only (ADR-0035, ADR-0096) | `node-addon-system` (`landlock-run`) | partial, owner 2026-10-10: plan (seam: #709) | a sandbox-runner port with a Landlock rung behind the same argument builder (rank 3) | PID namespace kills every descendant; credential masks follow workspace mounts |
| Workspace path helpers and file addresses | `Workspace::resolve` and `display` (`p1-workspace`) | — | `dsh-util-workspace-path` | partial | no address scheme; file addressing for ACP clients is open (#687) | one confinement point for every file tool (ADR-0025) |
| Syntax-highlighting language table | none | — | `dsh-util-code-language` | missing | a language hint on `read` results; belongs to an ACP client | — |
| Zero-trust client time zone | `contracts/clock.rs` (UTC ms only) | `p1-journal` | `dsh-util-time` | missing | per-session client zone; whether needed is unknown (ACP `initialize` could carry it) | — |
| Nominal (branded) identifiers | contract ids are plain `String`; newtypes only in `p1-workers`, `p1-workflow` | — | `dsh-brand` | partial | newtype the contract ids, call id first (zero runtime cost) | — |

## Family 3: agent loop, prompt and context (17 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Agent handle, live registry and event vocabulary | `AgentParts` (`p1-core`); `contracts/policy.rs` (`EventSink`, `AgentEvent`) | `p1-core`, `p1-host` | `dsh-agent` | different-by-design (ADR-0071, AGENTS.md: no registry) | none wanted: no step-interception waterfall | a tool not in `AgentParts` cannot dispatch |
| Agent loop: turn, step, tool scheduling, cancellation, failure recovery | `p1-core` `Agent::run_turn` over the contracts (`Provider`, `Tool`, `CommitSink`, policies) | `p1-core` | `dsh-agent-loop` | built | no request-error listener (retry is fixed in the provider crate and route files) | two-level overlap safety (tool says Shared and effect must not write); interrupted calls reconciled and tested |
| Mid-turn input queue (steering and notifications) | `Inbox` (`p1-core`); `contracts/history.rs` (`InboxKind`) | `p1-core` | `dsh-agent` (`Inbox`), `dsh-agent-loop` | partial | edit, remove, reorder, clear; next-step versus next-turn; journal before delivery (rank 21) | `Send` handle, never blocks, one owner |
| Default model for new agents | `settings.toml default_model`, `--model` (ADR-0049) | `p1-host` | `dsh-agent-default-model` | built | no programmatic saved selection | model refs validated against the catalog before a run or switch |
| Workspace instruction files (AGENTS.md chain) | `[instructions]` settings and loading in `p1-assembly` (ADR-0151) | `p1-assembly` (global file, then nearest git root to workspace, byte budget, notice), `p1-workspace` (protected open) | `dsh-agent-instructions` | built | change notices while running; whether loaded paths reach the journal environment record is unknown (rank 2 closed by #700) | an environment can switch instruction files off; over-budget files are truncated with a visible notice |
| System prompt assembly | prompt template with fixed placeholders (`p1-assembly`) | `p1-assembly` | `dsh-system-prompt` | partial | runtime-context sections (time, sandbox, approval, delegation); no escape for a literal `{{` | a prompt cannot name a tool the agent lacks; assembly is pure |
| Per-agent persona | `environments/<name>/prompt.md` | — | `dsh-persona` | built | no `{{model}}` placeholder | persona fixed by grant; cannot be shadowed at run time |
| Agent presets and preset roster | `environments/<name>/environment.toml`, `environments/subagents.toml` | `p1-assembly`, `p1-host` | `dsh-agent-preset-registry`, `dsh-agent-preset` | different-by-design (ADR-0071: an environment can only name host-registered modules) | a preset roster for a client (ACP config options #675) | an environment file cannot mount code |
| Tool presentation per agent (native schemas vs PTC) | none | — | `dsh-agent-tool-presentation` | optional, owner 2026-10-10: plan (seam: #714) | an assembly-time `[tools] presentation` key; dsh composes it only in the web-app `ptc` preset | — |
| Current time context | `{{date}}` filled at assembly; `contracts/clock.rs` | `p1-assembly` | `dsh-time-context` | optional | time of day, zone, elapsed time, refresh (dsh mounts it only in the experimental schedule bundle); the date goes stale in a long session | — |
| tmux location context | none | — | `dsh-tmux-context` | optional | pane context for an agent in tmux (dsh mounts it in no bundle); the owner's fleet tooling lives outside p1 | — |
| Context pressure and token measurement | `contracts/provider.rs` (`Usage`), `contracts/policy.rs` (`ContextInput.last_usage`); `p1-context::estimate_tokens` | `p1-context`, `p1-journal` (usage per response) | `dsh-token-meter` | partial | a shared measure with system, tools and messages breakdown and a per-turn fold (rank 30) | unknown usage is `None`, never zero (ADR-0019) |
| Repeated identical tool call reminder | none | stall guard for headless runs (ADR-0055) | `dsh-repeat-tool-reminder` | missing | same-call loop detection and the nudge (rank 18) | — |
| Plan mode | none | read-only environments; `ask_user_question` (ADR-0116, ADR-0135) | `dsh-plan-mode` | missing | toggle, planning guidance, reviewed exit, durable mode state (rank 16) | plan-only can be enforced by assembling no write tools, where dsh's is advice |
| Persisted long-running goal | none | `finish` plus bounded continuation (ADR-0037, ADR-0120) | `dsh-goal`, `dsh-tool-goal`, `dsh-command-goal`, `dsh-goal-round-driver` | missing | cross-turn objective with phases, human-gated rounds, `/goal` (rank 28) | `finish` is an observable end act; a worker result says whether it was verified; Python/Node checks count by recorded exit status, shell re-entry is refused (#734) |
| Slash command registry | `contracts/frontend.rs` (`CommandInfo`, `CommandOutput`, `SessionHandle::commands` and `command`): a closed list built by the host | `p1-host` `frontend_port/commands.rs` (the host's commands and one per skill); `p1-acp` `commands.rs` publishes it with `available_commands_update` and routes `/name` (#676); frozen TUI | `dsh-commands` | built | no plug-in point for new commands (not a recorded decision) | discoverable by any ACP client without a registry; `/model` and `/effort` share the config-option path |
| Manual compaction command | `contracts/policy.rs` (`ContextPolicy::compact_now`) | `p1-context`; `--resume --compact`; TUI `/compact` (ADR-0076) | `dsh-command-compact` | built | — (ACP `/compact` landed with #676) | one policy method serves the threshold and the manual path |

## Family 4: compaction, skills and attachments (14 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Context compaction (summarize old history, keep the recent tail) | `contracts/policy.rs` (`ContextPolicy`) | `p1-context`, `modules/p1-module-context`, host `SummaryService`; `[context]` per environment | `dsh-compaction`, `dsh-compaction-basic` | partial | threshold as a share of the routed window, summarizer on another model, range compaction (rank 19) | the replacement is journalled before it is installed; an over-window summary request is refused |
| Overflow recovery (provider says context too long: compact, then retry) | `contracts/provider.rs` (`ProviderErrorKind::ContextWindowExceeded`) | none: the kind is only displayed | `dsh-compaction-basic` | missing | compact once and retry on a provider-confirmed overflow (rank 1) | — |
| Manual compaction on demand | `ContextPolicy::compact_now` | TUI, CLI and ACP doors (ADR-0076, #676) | `dsh-compaction` (`compactNow`) | partial | range compaction | — |
| Compaction durability and busy lock | `contracts/journal.rs` (`ContextReplaced` record) | `p1-core` (commit before install), `resume.rs` | `dsh-compaction` | built | none | no lock bracket to orphan; the journal is the single truth |
| Tool-result pruning before summarizing | `[context] trim_at_tokens` (ADR-0127, ADR-0136) | `p1-context` | `dsh-compaction-tool-result-pruner` | built | the trim marker could name the stored-output handle for `read_output` | — |
| Image offload (swap over-budget images for placeholders, retry) | none: no image content in `contracts/history.rs` | — | `dsh-compaction-image-offload` | missing, owner 2026-10-10: plan (seam: #710) | a context-policy reaction to `ImageOffloadRequired`; needs image input first | — |
| Skill catalog and loading (index, body on demand) | skill summary and loaded-skill types plus the source trait in `contracts/skill.rs` (ADR-0151); `modules/wit/skills.wit` (`skills`: list, load) | `p1-skill-fs` (disk source, own two-field front-matter reader), `p1-tool-skill` (module `p1-module-skill`), joined by `p1-host` through `p1-assembly`'s source factory | `dsh-skill`, `dsh-skill-filesystem`, `dsh-tool-skill` | built | a journal record of the loaded skills is unknown (rank 13 closed by #700) | no YAML library; a frozen body snapshot per assembly; the guest chooses no path |
| Skill invocation policy and the user's `/name` gesture | `SessionHandle::commands` and `command` (`CommandOutput::Prompt`) | ACP `/NAME`: a turn that asks the model to load the skill through the environment's skill tool (#676, `p1-host` `frontend_port/commands.rs`) | `dsh-skill`, `dsh-skill-filesystem`, `dsh-tool-skill` | partial | per-skill visibility; extend #129 | the body still reaches the model only through the skill tool |
| Skill catalog refresh while running | none | index built once at launch | `dsh-skill-filesystem`, `dsh-skill`, `dsh-tool-skill` | missing | live refresh; a body hash in the journal environment record | — |
| Bundled Office-document skills | none | — | `dsh-skill-office` | missing, owner: not now | would ship as plain `SKILL.md` directories read by `p1-skill-fs` | — |
| `@file` mention completion | none | — | `dsh-file-reference`, `dsh-file-reference-local` | missing | clients own completion; p1 only needs to accept references (`embeddedContext` is false in #687) | — |
| File attachments (any file, stored verbatim) | none: `Item::User` is text only | — | `dsh-attachment`, `dsh-attachment-local` | missing, owner 2026-10-10: plan (seam: #710) | an `AttachmentStore` port, `FileRef` in history, verified reads | pinned-directory and exchange writes already exist (ADR-0108, ADR-0111) |
| Image attachments (validate, normalize, per-route versions) | none: no image type in `contracts/provider.rs` or `history.rs` | — | `dsh-attachment`, `dsh-attachment-local` | missing, owner 2026-10-10: plan (seam: #710) | image input end to end (rank 8): `ImageRef` in history, per-route projection; the ACP part is #694 | — |
| Office document conversion and rendering | none | — | `dsh-office-to-pdf`, `libreoffice-kit` | missing, owner: not now | whole capability | — |

## Family 5: model providers and credentials (12 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Provider-neutral model call and adapter registry | `contracts/provider.rs` (`Provider`: describe, validate, stream; one terminal event, ADR-0017) | `p1-provider-anthropic`, `p1-provider-openai`, `p1-provider-openai-chat` (modules `p1-module-provider-*`), `p1-provider-http`, `p1-provider-conformance`; route `adapter` field mapped by `p1-host` | `dsh-llm` | built | one provider per agent (the summarizer uses the same one) | closed error enum; diagnostics never carry a credential or body; no dynamic registration |
| Model catalog, discovery and per-model capabilities | `p1-model-profile` (`profiles/*.toml`), route `[models]` bindings | `p1-model-profile`, `p1-host` (`p1 models`) | `dsh-llm`, `dsh-llm-deepseek`, `dsh-llm-pi-ai` | partial, owner 2026-10-10: plan (seam: #710) | no live endpoint catalog; the input-modality flag in the profile comes with image input | a route serves only the profiles it names: no unknown id reaches a paid endpoint |
| DeepSeek Messages wire adapter (direct official API) | `Provider` port | OpenCode Go routes only (ADR-0134, ADR-0138) | `dsh-llm-deepseek`, `dsh-llm-deepseek-api-key`, `dsh-llm-deepseek-account` | partial, owner 2026-10-10: plan (seam: #712) | no direct `api.deepseek.com` route yet: a dialect arm, a route file and an account file; no account route by decision; the Files API belongs to #710 | endpoint origin compiled in; a route cannot send its credential elsewhere (ADR-0110) |
| Multi-provider adapter over a vendor catalog | `Provider` port plus route files (ADR-0039) | one adapter crate per wire protocol | `dsh-llm-pi-ai` | different-by-design (ADR-0039, ADR-0110) | none wanted | no ambient credential discovery; origin checked before any credential read |
| Request retry policy and executor | `RetryPolicy` (`p1-provider-http/src/drive.rs`); route `retry_policy` presets (ADR-0137, ADR-0145) | `p1-provider-http` | `dsh-llm-retry`, `dsh-llm` | partial | step-level re-run after partial output; a retry record before the wait (rank 7) | permanent failures are never retried; a hostile Retry-After is capped |
| Provider-specific extra request fields | typed `[adapter_settings]` in the route file | `p1-host` (settings to the provider component) | `dsh-deepseek-llm-api-extensions` | different-by-design (ADR-0093: typed route settings, host-validated; AGENTS.md: no registry) | none wanted | no module can add a field to the provider body |
| Credential store and lookup (references, not values) | `p1-auth` (`resolve`, `describe`; chain per route, ADR-0040; pinned directory, ADR-0108) | `p1-auth`, `p1-redact` | `dsh-credentials`, `dsh-credentials-local` | built | no generic named-record store for modules; no `.env` file fallback (not a recorded decision) | pinned-directory writes; sources bound to compiled origins; `Debug` prints `<redacted>` |
| Credential for a route that needs none, or a proxy injects it | `[credential] kind = "none"` (ADR-0070) | `p1-auth`, `p1-provider-http` | `dsh-credentials` (undefined), `dsh-llm-pi-ai` (keyless) | built | none | explicit declaration instead of ambient discovery |
| Account separate from route (several accounts per provider) | `accounts/*.toml` (ADR-0139) | `p1-host` (`accounts.rs`) | `dsh-llm-deepseek-api-key`, `dsh-llm-deepseek-account`, `dsh-deepseek-account` | built | none | several accounts on one route |
| Account sign-in state, profile and balance | `UsageProbe` (`p1-usage`) | `p1-usage` | `dsh-deepseek-account`, `dsh-deepseek-account-platform` | different-by-design (owner 2026-10-10: the direct DeepSeek API route only; no account service) | none wanted | probes carry no credential; `Unsupported` is shown, never an invented number |
| Human-guided credential flows (browser PKCE sign-in, code entry) | `p1 login` (`host/login.rs`), `p1-auth` | `p1-host` | `dsh-authorization`, `dsh-deepseek-account-platform` | partial | a browser or OAuth login for any route, a flow port, a loopback callback server (rank 14; `docs/design/credentials.md` §8.4) | a key is taken from stdin only; OAuth entries never copied between tools |
| Failure classification and account-scoped failure codes | `contracts/provider.rs` (`ProviderErrorKind`) | `p1-provider-http` (`classify_status`) | `dsh-llm`, `dsh-llm-deepseek-account` | built | no sign-in-required versus token-invalid split; `Transport` not split | reset time parsed from vendor hints |

## Family 6: tool runtime and built-in tools (15 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Tool contract, registry and execution pipeline | `contracts/tool.rs` (`Tool`), `contracts/policy.rs` (`AuthorizationPolicy`) | tools as WebAssembly modules (`modules/p1-module-*`); `p1-assembly` resolves names; `p1-redact` (`RedactingTool`) | `dsh-tools` | built | no pre- or post-execute listener chain (not a recorded decision); a `deferLoading`-style marker unknown | tool code runs in a WebAssembly sandbox with capability grants; `effect()` classifies each call before it runs |
| Parallel tool calls in one response | `contracts/tool.rs` (`Concurrency`, `Tool::concurrency`, ADR-0118) | `p1-core` scheduler; `[tool_concurrency]` | `dsh-tools` | built | none for native calls | read-only shell commands can overlap; per-environment cap |
| Programmatic tool calling (`run_code`) | none | workflows differ (ADR-0053) | `dsh-tools` | optional, owner 2026-10-10: plan (seam: #714) | a `run_code` tool module over a `program-runtime` capability (dsh composes it only in the web-app `ptc` preset) | — |
| Per-call tool time limit | none generic: shell `timeout_seconds`; module deadlines (ADR-0112) | `p1-tool-shell`, `p1-module-runtime` | `dsh-tool-call-timeout-policy` | partial | a uniform budget declaration; moot while only `shell` takes a time | a hard process-group kill by the host, not a cooperative signal |
| File read, write and targeted edit | `Tool` port plus file capability services (`runtime/`, ADR-0088, ADR-0095) | `p1-tool-read`, `p1-tool-edit`, `p1-tool-write`, `p1-tool-patch` (modules `p1-module-read`, `-edit`, `-write`, `-patch`) over `p1-workspace` | `dsh-tool-fs`, `dsh-tool-str-replace-editor` | built | directory `view` and `insert` inside one editor tool (p1 has `ls` and `ToolFace` variants) | read-before-mutate always on; leaf exchange; several files or ranges per `read` |
| Image reading | none | — | `dsh-tool-fs` (`read_image`) | missing, owner 2026-10-10: plan (seam: #710) | a `read_image` tool module over an `attachments` capability, with image input (#694) | — |
| File and content search | `Tool` port | `p1-tool-search` (`grep`, modes `files` and `count`), `p1-tool-ls` (ADR-0115) | `dsh-tool-fs-search` | built | modification-time ordering, over-cap sampling, recoverable capped results | pagination arguments in the schema |
| One-shot shell with background jobs | `Tool` port plus `modules/wit/jobs.wit` (`process-jobs`; `runtime/` `ProcessJobsService`) | `p1-tool-shell`, `p1-tool-shell-job` (modules `p1-module-shell`, `p1-module-shell-job`) | `dsh-tool-bash`, `dsh-tool-jobs` | partial, owner 2026-10-10: plan (seam: #696) | jobs for other producers, a wake cap, a blocking wait; model-requested sandbox escalation with user approval | bubblewrap boundary with credential masks; the host-observed exit code is the only evidence |
| Persistent shell session | none (ADR-0117 excludes it) | — | `dsh-tool-bash-persistent`, `dsh-tool-pwsh-persistent` | missing | cross-call shell state; see the PTY row (rank 24) | — |
| PowerShell one-shot shell | none | — | `dsh-tool-pwsh` | missing, owner 2026-10-10: plan (seam: #709) | a second shell tool module over the `process` capability, on Windows | — |
| Tool output retention (spill) | `tool-outputs` capability (`runtime/` `OutputStore`, ADR-0109) | `p1-tool-read-output` (module `p1-module-read-output`), shell | `dsh-spill`, `dsh-spill-local`, `dsh-spill-policy` | partial | spill for non-shell tools, orphan cleanup, a backend port (rank 12) | redacted before it reaches disk; random handles, never paths |
| Asking the user a question | `user-questions` interface (ADR-0116); host `QuestionAsker` (`host/questions.rs`, host composition) | `p1-tool-question` (module `p1-module-question`), `runtime/` `UserQuestionsService` | `dsh-user-questions`, `dsh-tool-ask-user` | partial | timed or pending mode, late answers, an open-questions projection (rank 29); ACP `elicitation/create` #674 | workers can ask without owning the screen; the call is gated on a user invitation (ADR-0135) |
| Task list tool | none | — | `dsh-tool-todo` | missing | the tool and its journal event (rank 10); #692 uses workflow execution steps for ACP `plan`, not an agent-authored todo source; the tool gap remains separate | — |
| Declaring deliverable files (`present`) | none | `finish` (ADR-0037) | `dsh-tool-present` | missing | whole tool; depends on a deliverables view in a client | — |
| Bundled script-runtime paths | none | — | `dsh-tool-workspace-dependencies` | optional | only for a product that ships its own runtimes (dsh mounts it in `dsh-sdk-app` only, disabled unless an environment variable is set) | — |

## Family 7: web, MCP, workflows and programmatic tool calling (11 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Web access service (search and fetch behind one provider seam) | none; #516 plans a scoped `web` interface in `modules/capabilities.toml` with a host-owned backend | — | `dsh-web` | missing, owner 2026-10-10: plan (seam: #717) | whole capability (rank 5): a `web` WIT capability and host-side `SearchBackend` and `FetchBackend` ports; #516 deferred by the owner (D27, 2026-10-05) | tools cannot reach arbitrary URLs today: every network grant is a fixed-origin host capability (ADR-0130) |
| Anonymous public URL fetch with SSRF protection | none | — | `dsh-web-fetch-http` | missing, owner 2026-10-10: plan (seam: #717) | a `FetchBackend` with public-address check, connection pinning, redirect policy and caps (#516 definition of done) | — |
| Web search backend (native DeepSeek search) | none | origin-bound credentials in `p1-auth` reusable | `dsh-web-search-deepseek` | missing, owner 2026-10-10: plan (seam: #717; a DeepSeek search adapter itself is not ordered: the owner chose the direct API route only) | one `SearchBackend` adapter behind the web port | credentials bound to an origin and masked everywhere |
| Model-facing web tools (`web_search`, `web_fetch`) | none; planned `web_search`, `read_web_page` (#516) | — | `dsh-tool-web` | missing, owner 2026-10-10: plan (seam: #717) | both tool modules, untrusted-content labelling, HTML to text (rank 5) | web access can be withheld per role through grants (ADR-0050) |
| MCP client bridge (external tool servers) | none; a tool adapter behind the `Tool` contract (#695 names it as its own issue) | — | `dsh-mcp-client` | missing, owner 2026-10-10: plan (seam: #695) | whole client (rank 6): an `McpClient` port, one native adapter, tool generations named `mcp__<server>__<raw>`; the environment names the server, never its tools | — |
| MCP resource discovery and reading | none | — | `dsh-mcp-resources`, `dsh-mcp-client` | missing, owner 2026-10-10: plan (seam: #695) | a tool module over an `mcp-resources` capability; depends on the MCP client | — |
| Workflow run service, events and ownership | `WorkflowService` (`p1-workflow/src/api.rs`: `StartRuns`, `ObserveRuns`, `CancelRuns`, `WorkflowObserver`); host implements `StepRunner`, `ModelResolver` | `p1-workflow`, `p1-host` | `dsh-workflow` | built | none | run journal with `resume_from`; per-model attempt caps and role fallback chains |
| Workflow script engine and sandbox | rhai engine in `p1-workflow` (ADR-0053, ADR-0114) | `p1-workflow` | `dsh-workflow-ptc`, `dsh-ptc-runtime-node`, `dsh-ptc-runtime` | different-by-design (ADR-0053) | none wanted (no JavaScript scripts) | scripts have no host access at all; caps are engine-enforced |
| Model-facing workflow tool | `Tool` port plus the `workflows` capability | `p1-tool-workflow` (modules `p1-module-workflow-start`, `-status`, `-result`, `-cancel`) | `dsh-tool-workflow`, `dsh-workflow-ptc` | built | no foreground mode (p1 always returns at once and notifies; not a recorded decision) | separate status, result and cancel tools; the module holds only `control` and `workflows` |
| PTC runtime (model-written programs that call tools) | none | — | `dsh-ptc-runtime`, `dsh-ptc-runtime-node` | optional, owner 2026-10-10: plan (seam: #714) | a `ProgramRuntime` port with one runtime per language (dsh serves it only in the web-app `ptc` preset) | — |
| Ralph loop (fresh agent per round toward one objective) | none | workflow scripts with verifier and judge roles | `dsh-tool-ralph`, `dsh-workflow-ptc`, `dsh-workflow` | optional | the packaged loop (dsh ships its base row `disabled: true`); expressible as a rhai script today | — |

## Family 8: file access, shell, sandbox and processes (10 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| File access service (resolve, read, list, atomic write, edit) | `runtime/capabilities.rs` (`WorkspaceService`, `SnapshotService`, `MutationService`) plus `modules/wit/workspace.wit`; not in `contracts/` | `runtime/file_services.rs` over `p1-workspace` (one backend) | `dsh-fs`, `dsh-fs-local` | partial, owner 2026-10-10: plan (seam: #713) | one backend; the remote backend is decided, so the traits move to `contracts/` when it is built; no watch | leaf replacement by exchange refuses a swapped-in entry; one `WriteGate` for all agents (ADR-0032) |
| Read-before-edit and stale-file guard | `ObservedFiles` (`p1-workspace`), `SnapshotService` | `edit`, `write` under the `WriteGate` (ADR-0025) | `dsh-fs-observation-policy` | built | no confirmed-absent state; whether observed state survives resume is unknown | freshness by content hash, not metadata; cannot be removed by configuration |
| File write fence and workspace confinement | `Workspace::resolve` (`p1-workspace`; ADR-0025, ADR-0122) | — | `dsh-fs-sandbox`, `dsh-sandbox-policy`, `dsh-fs-local` | different-by-design (ADR-0025) | a read-only file mode for review environments (no demand evidence); reads outside the workspace (#76) | reads are confined too; credential files unreadable; descriptor-based walk |
| Shell command execution (one-shot commands) | `Tool` port plus the `process` capability (`runtime/` `ProcessService`) | module `p1-module-shell` (`p1-shell-guest`), `p1-tool-shell` | `dsh-shell`, `dsh-bash-local`, `dsh-bash-sandbox` | built | no executor port for a non-bash runner (one implementation); no per-call cwd or stdin | closed request type (no env, cwd or sandbox per call); exit evidence is host-observed (ADR-0102) |
| Foreground timeout and background hand-over | `modules/wit/jobs.wit` (`process-jobs`) | `runtime/` `ProcessJobsService`, `p1-tool-shell-job` (ADR-0117, ADR-0123) | `dsh-shell` (`onExpiry`), `dsh-bash-local` | built | none | — |
| Managed shell environment (trusted facts for every command) | `ENV_ALLOW` allow-list in `runtime/` `ProcessService` (`with_env_pass`) | `p1-host` (`shell_entry`) | `dsh-shell-env` | partial | no session id or profile facts reach the shell (no consumer found) | allow-list instead of a name heuristic; injected snapshot, tests never read the real environment |
| Process sandbox: modes, per-call policy, fail-closed, escalation | none: a concrete `Sandbox` struct (`runtime/process/sandbox.rs`) and `--sandbox` flags | bubblewrap | `dsh-sandbox`, `dsh-sandbox-policy`, `dsh-bash-sandbox` | partial, owner 2026-10-10: plan (seam: #696) | a `SandboxModes` port: read-only mode, per-session modes, approved escalation, an unsandboxed mode, denial facts (rank 4) | home hidden behind a tmpfs; credential directories refused; a request cannot switch the sandbox off |
| Platform sandbox runners (bwrap, Landlock, Seatbelt) | none: a bubblewrap argument builder | `scripts/ci-bwrap.sh` (ADR-0097) | `dsh-sandbox-local`, `dsh-sandbox-windows-acl` | partial, owner 2026-10-10: plan (seam: #709) | a `SandboxRunner` port with a Landlock rung (rank 3), a Seatbelt runner and a Windows runner | launcher vetted against workspace-writable roots and re-validated per command |
| Subprocess service (managed process range, bounded output, escalated kill) | `runtime/capabilities.rs` (`ProcessService` trait) | native `runtime/process/mod.rs` (ADR-0091) | `dsh-subprocess`, `dsh-subprocess-local` | partial, owner 2026-10-10: plan (seam: #709) | one adapter per OS: cgroup or scope ownership for unsandboxed runs, terminal spawn (rank 11) | PID namespace when sandboxed; group-id reuse guarded |
| Persistent PTY terminal sessions | none (ADR-0117 excludes it) | — | `dsh-terminal`, `dsh-terminal-bash` | missing | PTY allocation, interactive input, signals, per-agent ownership (rank 24) | — |

## Family 9: approval, hooks, jobs and workspace (9 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Per-call authorization and ask (one-shot user approval) | `contracts/policy.rs` (`AuthorizationPolicy`, `Decision`); host `Asker` and `AskBridge` (`host/policy.rs`, host composition) | policy modules `p1-module-policy-full-access`, `p1-module-policy-ask`; `p1-host`; `p1-acp` (`session/request_permission`, ADR-0154) | `dsh-user-approval` | built | no separate ask audit record in the journal (unknown) | the policy sees the call and its `Effect`; `Always` grants are keyed to verified digests and void on replacement |
| Permission presets (one selector over sandbox plus approval) | two flags fixed at start (`--ask`, `--sandbox`); per-session approval mode `ask` / `read-only` / `full-access` over the assembled policy (`p1-host` `frontend_port/mode.rs`, #696), bounded by `--ask` | `p1-acp` (`session/set_mode`, `mode` config option, #696) | `dsh-permission-presets`, `dsh-user-approval`, `dsh-sandbox-policy` | partial, owner 2026-10-10: plan (seam: #696) | named presets in the environment file; a per-session sandbox mode (dsh `workspace-write` vs `danger-full-access` is fixed by `--sandbox`) (rank 26) | confinement and credential refusal cannot be weakened by a preset |
| Model-based per-call authorization review | the `AuthorizationPolicy` port can host it | — | `dsh-experimental-auto-review` | optional | whole capability (dsh: experimental, web profile only, shipped switched off); would need a new capability grant | — |
| Hook protocol core (matcher, codec, merge, `hook/*` events) | none; the decision points are `AuthorizationPolicy` and `ContextPolicy` | `p1-hook-shadow` (detached, fail-open, ADR-0058) | `dsh-hook-protocol` | optional, owner 2026-10-10: plan (seam: #711) | a `HookRunner` core wrapped around the assembled policies: block, inject context, force continuation (dsh: a library used only by the two bridges, neither in a shipped bundle) | the one existing hook cannot affect a run or leak into the model |
| Claude Code hook compatibility bridge | none | — | `dsh-hooks-claude-code`, `dsh-hook-protocol` | optional, owner 2026-10-10: plan (seam: #711) | the hooks-file reader and event mapping over the core (dsh: in no shipped bundle, added by hand) | — |
| Codex hook compatibility bridge | none | — | `dsh-hooks-codex`, `dsh-hook-protocol` | optional, owner 2026-10-10: plan (seam: #711; this bridge is optional) | a second reader over the same core (dsh: in no bundle) | — |
| Background jobs: registry, producers, observers | `modules/wit/jobs.wit` (`process-jobs`); `runtime/jobs.rs` (`JobRegistry`, `JobObserver`) | `p1-tool-shell-job`; host inbox notification | `dsh-jobs`, `dsh-jobs-local`, `dsh-tool-jobs` | partial | jobs only for shell commands; no per-session cap; no wait verb; stored output unbounded | output redacted before storage; a job inherits the spawn's sandbox decision |
| Workspace (project) registry and session grouping | none | host session path helpers (`host/session.rs`) | `dsh-workspace` | missing | project list, grouping, archive; likely a client concern (ACP `session/new` carries cwd); undecided | — |
| Per-turn workspace change summary and per-file comparison | `WorkspaceMutations` counter (`p1-workspace`) | `p1-host` (`activity.rs`, ADR-0055) | `dsh-workspace-changes` | missing | per-turn file list with line counts and per-file comparison (rank 17); clients may render from tool results | the change signal needs no git, subprocess or file copies |

## Family 10: sessions and storage (12 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Session event log (append-only, history derived, compaction hides) | `contracts/journal.rs` (`CommitSink`, `JournalRecord`, `RecordBody`); `p1_core::resume::project` | `p1-journal` | `dsh-session` | partial | no fork, no list or get, no per-event metadata; ACP #691 needs load, list, resume and fork | a smaller contract (nine record kinds); dense-sequence check at the store |
| Durable session store and JSONL backend | `CommitSink` | `p1-journal` (`MemoryJournal`, `JsonlJournal`, `SyncPolicy`) | `dsh-session-persistence`, `dsh-session-persistence-jsonl` | partial | a store keyed by session id under a root; list, stat and read-only handles (#62, #691) | 0600 at creation, fsync per record, lock before read; refuses files above 256 MiB |
| Durability checkpoints before requests and side effects | the core commit order (ADR-0021, ADR-0022) | `p1-core` | `dsh-session-checkpoint-policy` | built | none | part of the core order: a composition cannot omit it |
| Resume of a stored session | `Agent::resume` (`p1-core`; ADR-0033, ADR-0034) | `p1-host` | `dsh-session`, `dsh-session-persistence-jsonl`, `dsh-session-format` | built | none | refuses resume on another route or model; reports tool-identity changes |
| Session format versioning and migration chain | `p1-journal` header `p1_journal: N` | `p1-journal` | `dsh-session-format`, `dsh-session-format-catalog`, `dsh-session-format-v0-to-v1` and later | partial | no migration chain; not a recorded owner or ADR decision; not needed today (old versions are read in place) | never rewrites a file's header or contents |
| Session listing, filtering, event reads, lineage, search | none | — | `dsh-session-query`, `dsh-session-query-sqlite` | missing | list, filters, lineage, search (rank 23); #62 (epic #72), #517; ACP #691 | — |
| Session titles (fallback, optional LLM, rename) | none | — | `dsh-session-title`, `dsh-session-title-llm`, `dsh-session-title-first-prompt-llm` | missing | a fallback title, rename, journalled title; ACP `session_info_update` #693 | — |
| Projection registry and persisted projection cache | one projection (`project`) | `p1-acp` maps live events (#687) | `dsh-session-projection`, `dsh-session-projection-cache` | missing | a snapshot with sequence, a change feed, a cold cache; needed for ACP `session/load` (#691) | — |
| Whole-session statistics and turn outline | `RequestTiming` records (ADR-0121); `scripts/journal-timing.py` | `p1-journal` | `dsh-session-stats`, `dsh-session-turn-outline` | partial | live per-session counters and a turn outline; ACP usage updates #677 | unknown is printed as unknown, never zero |
| Cross-session references (another session as untrusted context) | none | — | `dsh-session-reference` | missing | depends on session query; #517 deferred | — |
| Incremental session-log upload to the official DeepSeek API | none | — | `dsh-session-log-deepseek` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | — |
| Durable host-side key-value storage | none: each persistent file has one owner | `p1-auth` store, `p1-workspace` `write_atomic`, journals | `dsh-storage`, `dsh-storage-domain`, `dsh-storage-json` | different-by-design (owner 2026-10-10: lead batch, each persistent file has one owner; no shared store or backend table) | none wanted until a second host-side store (a session index) exists; then decide | no hub, no process-wide backend table, no global store to migrate |

## Family 11: telemetry and feedback (8 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Session-event capture seam with redaction before hand-off | `contracts/policy.rs` (`EventSink`), `contracts/journal.rs` (`CommitSink`) | `p1-journal`, `p1-redact` | `dsh-session-telemetry` | different-by-design (owner 2026-10-10: never; no data leaves the machine, so no outbound capture seam) | none wanted: local capture is the `EventSink` and `CommitSink` ports | credentials masked at the tool boundary, so the canonical log is clean |
| OpenTelemetry (OTLP) reporting channels | none | — | `dsh-otel`, `dsh-session-telemetry-otel`, `dsh-host-product-telemetry-otel` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | default-quiet: nothing leaves the machine |
| Feedback-authorized session-log upload | none | — | `dsh-session-telemetry-otel` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | p1 uploads nothing |
| Product usage events (explicit analytics) | none | worker-observability rules (docs), `p1-usage` quota ledger | `dsh-host-product-telemetry-otel` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | analytics rules stricter in writing (no prompts, secrets or tool contents) |
| Anonymous installation identity | none | — | `dsh-anonymous-user-id` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | no install id on provider requests |
| Per-message ratings and notes | none | — | `dsh-message-feedback` | missing (owner 2026-10-10: not now; local-only ratings possible later, never uploaded) | local ratings not now | — |
| Session feedback command and dialog | none | — | `dsh-command-feedback` | missing (owner 2026-10-10: not now; local-only ratings possible later, never uploaded) | local ratings not now | — |
| Session-log download (ZIP) | journals on disk; `scripts/run-report.py` | — | `dsh-session-log-export` | partial | an export command or archive through an ACP client (a local export; the owner's "never" of 2026-10-10 covers upload, not this) | no HTTP route to protect |

## Family 12: subagents, teams, schedule and experimental (9 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Start a child agent and get its result (one-shot delegation) | `WorkerService`, `WorkersStart` (`p1-workers/src/lib.rs`; optional module, ADR-0026); not in `contracts/` | `InProcessWorkers`; `p1-tool-delegate` (modules `p1-module-worker-start`, `-result`, `-continue`, `-cancel`, `p1-module-task`); `environments/subagents.toml` (ADR-0131) | `dsh-subagent`, `dsh-subagent-spawn-in-process`, `dsh-subagent-in-process-driver`, `dsh-tool-subagent` | partial, owner 2026-10-10: plan (seam: #715) | background or foreground choice, depth above one; external backends through a `WorkerBackend` port, and the worker port moves to `contracts/` when the second backend is built | an exact grant list; `worker_*` tools are never grantable (no recursion) |
| Per-call child model and effort choice | `ChildOptions`, `SubagentRequest` (ADR-0131) | `p1-workers`, `p1-host` | `dsh-tool-subagent`, `dsh-subagent-spawn-in-process`, `dsh-subagent-fork-in-process` | partial | a `list_subagent_models` tool; a route allow-list inherited by children | allowed models are a closed list per configured subagent |
| Fork: child seeded with the parent's completed turns | none | — | `dsh-subagent-fork-in-process` | missing | a child that continues the parent's conversation (ACP `session/fork` #691 is a session fork, a different thing) | — |
| Continue, steer, interrupt and list children | `worker_continue`, `worker_cancel`, `worker_result`; `WorkersObserve::list` exists on the trait | `p1-tool-delegate` | `dsh-tool-subagent-control`, `dsh-subagent` | partial | steer a running child, child-to-parent messages, a list tool, durable children (rank 22); #679 is main-turn steering | continuing a child cannot race a running turn |
| Agent teams (lead plus teammates, mailbox, task board) | none | workflows plus an ACP client | `dsh-experimental-agent-team`, `dsh-experimental-tool-agent-team`, `dsh-experimental-client-ui-agent-team`, `dsh-experimental-agent-team-profile` | optional, owner: not now | whole capability (dsh: experimental, shipped switched off, "an unstable prototype") | — |
| Scheduled reminders (once, interval, daily, weekly, cron) | none | an external scheduler starting a headless run | `dsh-experimental-schedule-bundle`, `dsh-schedule` | optional, owner 2026-10-10: plan (seam: #716) | a `Schedule` port, store and timer driver over the front-end port (dsh: experimental schedule bundle, shipped switched off) | — |
| Inbound webhook that starts a new agent session | none | — | `dsh-webhook`, `dsh-webhook-github` | missing, owner 2026-10-10: plan (seam: #716) | a `TriggerRule` port and a GitHub ingress adapter outside `p1-host` | — |
| Speech-to-text service with a local recognizer | none | — | `dsh-experimental-speech-to-text`, `dsh-experimental-speech-to-text-sensevoice`, `dsh-experimental-api-speech-to-text` | optional, owner: after ACP door | whole capability (dsh: experimental, shipped switched off) | — |
| Voice input in the front end | none | — | `dsh-experimental-voice-input-bundle`, `dsh-experimental-client-ui-voice-input` | optional, owner: after ACP door | a client feature (dsh: experimental, shipped switched off) | — |

## Family 13: front ends, ACP, API and host (14 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| ACP agent door (stdio, standard protocol) | `FrontEndPort` and `SessionHandle` in `contracts/frontend.rs` (ADR-0152), with neutral `WorkflowStep` observations (#692), `WorkflowProgress` and worker-end note hooks (#678) | `p1-acp` (mapping #687, own `wire::v1`, D6; the `p1 acp` driver, ADR-0154; several sessions and close, ADR-0156; standard flat workflow `plan` snapshots #692; cumulative workflow tool cards and worker end notes #678); `p1-host` composes it | `dsh-acp`, `dsh-acp-app` | in-flight #670 | #691 load, list, resume, fork; #693 titles; #694 images; #695 client MCP servers; #696 modes; an agent todo source remains missing (D8: done only at dsh ACP parity) | a published extension surface under `_meta["p1.dev"]`; every `AgentEvent` mapped; one session process per client folder |
| SDK JSON-RPC stdio server (own wire protocol) | none by decision: no second automation protocol (#670 D1) | — | `dsh-sdk-protocol`, `dsh-sdk-jsonrpc-server`, `dsh-sdk-app`, `dsh-sdk-minimal` | different-by-design (#670 D1) | none wanted | one automation protocol to keep in step |
| Headless one-shot run (script and CI use) | `Command::Run` (`host/cli.rs`), `run_headless` (`host/run.rs`) | `p1-host` line front end (ADR-0043) | `dsh-headless` | built | no `--json` event stream; resume by id (#62); task from stdin | distinct exit codes: blocked, stalled, usage, cancelled; wait after transient failure (ADR-0041) |
| HTTP server for a browser UI | none | — | `dsh-host-webserver`, `dsh-host-frontend-static` | missing, owner: after ACP door | none planned | — |
| Web GUI product (browser app, authenticated URL) | none | — | `dsh-web-app`, `dsh-web-frontend` | missing, owner: after ACP door | whole GUI; if built, an ACP client | — |
| Typed browser-to-host RPC (gateway and remote catalogue) | none by decision: ACP plus `_p1/` extensions | — | `dsh-api-gateway`, `dsh-api-remotes` | different-by-design (#670 D1, design choices 5 and 9) | reconnect to a running session (several sessions landed, ADR-0156) | one protocol to secure and version |
| Session and workspace navigation API | none (start-time flags) | — | `dsh-api-session-controller`, `dsh-api-workspace-controller` | missing, owner: after ACP door | session list, rename, archive (#62, #691); config options #675; workspaces as objects undecided | — |
| Human view of background jobs (list, follow, kill) | `runtime/jobs.rs` (`JobObserver`); `host/jobs.rs` | `p1-tool-read-output` | `dsh-api-job-controller` | partial | a human roster, follow and kill distinct from the model's; `_p1/agent_state` #683, `_p1/run/cancel` #682 | — |
| Human terminal pane (shell in the workspace) | none (a client concern under D1) | — | `dsh-api-terminal-controller` | missing | a client feature; no issue | — |
| Workspace file preview and watch (for the human) | `runtime/directory_listing.rs` (`DirectoryListingService`); the `read` tool | `p1-tool-read`, `p1-tool-ls` | `dsh-api-workspace-files` | missing | human preview and watch (client filesystem operations are not in the first ACP slice) | reads confined to the workspace plus one scratch root |
| Account, settings and credential screens | CLI: `p1 login`, `p1 logout`, `p1 usage` | `p1-host`, `p1-auth`, `p1-usage` | `dsh-api-account-controller`, `dsh-api-settings-controller` | partial | a redacted settings and credential surface for a client; screen-driven sign-in | no secret-returning path; no callback listener to secure |
| Workspace directory picker | none (the client sends `cwd` in `session/new`) | — | `dsh-host-directory-picker`, `-auto`, `-browse`, `-native` | missing, owner: after ACP door | a client feature | — |
| Open workspace in an installed app | none | — | `dsh-host-open-in-app` | missing, owner: after ACP door | a client feature | — |
| Plugin inventory (what is loaded, its state) | `p1 modules list`, `inspect`, `verify` (`host/modules_cli.rs`; ADR-0079, ADR-0087) | `p1-module-runtime` | `dsh-host-plugin-inventory` | partial | live state of a running host; a client-callable view | `verify` checks digests against the release manifest |

## Family 14: client UI (44 rows)

Checked by a smaller model only (survey `RUN.md`). Every row here is a client feature. p1's
own TUI and GUI are ACP clients (owner 2026-10-09; `p1-tui` and `host/tui.rs` are frozen), so
for most rows the port is the ACP wire plus the `_p1/` extensions and the adapter is a client,
not `p1-host`. "client" in the adapter column means exactly that.

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Browser-to-host transport (authenticated calls, live event stream) | ACP over stdio (ADR-0154); no HTTP listener | `p1-acp` | `dsh-client-connection` | in-flight #670, owner: after ACP door | no socket; if one is ever added, dsh's Host and Origin fence rules apply | no network listener today |
| Raw file upload into an agent's attachment area | none | — | `dsh-client-file-upload` | missing, owner 2026-10-10: plan (seam: #710) | attachment intake, after image and file input | — |
| Live reload of a rebuilt plug-in bundle into an open page | none | — | `dsh-client-hmr` | different-by-design (owner 2026-10-10: lead batch, p1 ships no browser plug-in bundle to reload, ADR-0071) | none wanted | — |
| Plug-in bundle loading in the browser (boot graph, lazy table, live enable) | module names in `environment.toml` (ADR-0071) | `p1-host` | `dsh-client-modules` | different-by-design (ADR-0071, owner 2026-10-10) | none wanted | nothing registers implicitly |
| Installing, enabling, disabling browser plug-ins; inventory | verified releases plus the environment file (ADR-0065, ADR-0079, ADR-0087) | — | `dsh-client-ui-plugin-manager`, `dsh-client-ui-settings-plugins`, `dsh-client-ui-settings-plugin-inventory` | different-by-design (ADR-0065, ADR-0079, ADR-0087) | none wanted | module identity verified before loading |
| Observable state stores shared by UI packages | the journal as single truth (ADR-0021) | — | `dsh-client-store` | different-by-design (ADR-0021) | a client store may only project journal or ACP events | one source of state |
| Unified resource addresses resolving to live client values | none; `read_output` handles are the nearest | `p1-tool-read-output` | `dsh-client-resources` | missing | an address resolver for clients; no decision | — |
| Typed UI extension points (slots) | `FrontEndPort` (`contracts/frontend.rs`, ADR-0152) | `p1-host` (line front end), `p1-acp` | `dsh-client-ui-slots`, `dsh-client-ui-renderer`, `dsh-client-ui-session` | different-by-design (#670 D1 and D7, ADR-0152: one explicit front-end port, no slot registry) | none wanted | default no-op hooks let a line front end ignore what it does not render |
| Window frame, panels and viewing-state service | none (frozen TUI) | client | `dsh-client-ui-layout`, `dsh-client-ui-primitives` | missing, owner: after ACP door | a new ACP client's job | the pure state machine and separate driver split (ADR-0043) is reusable |
| Light, dark and system theme with design tokens | none | client | `dsh-client-ui-theme` | missing, owner: after ACP door | client | — |
| Interface language (host-stored preference, dictionaries) | none | client | `dsh-client-locale` | missing, owner: after ACP door | client; no decision | — |
| Keyboard command registry and key routing | none | client | `dsh-client-shortcuts`, `dsh-client-ui-shortcuts` | missing, owner: after ACP door | client | — |
| Slash commands and `@` triggers in the composer | frozen TUI commands; CLI flags; ACP `available_commands_update` #676 | client | `dsh-client-ui-commands`, `dsh-client-ui-input-trigger`, `dsh-client-ui-reference` | partial | a typed command and reference menu in a client; `@file` references (#687 has `embeddedContext` false) | — |
| Conversation shell and chat (layout, composer, node renderers) | `contracts/policy.rs` (`EventSink`); `FrontEnd::event_sink` | `LineFrontEnd`; ACP session updates (#670) | `dsh-client-ui-conversation`, `dsh-client-ui-chat` | in-flight #670 | ACP client renderers | — |
| Tool call cards (call tree, per-tool presentation slot, skill row) | `Tool::describe`, `describe_result` (ADR-0057, ADR-0059) | ACP `tool_call` updates (#687) | `dsh-client-ui-tool`, `dsh-client-ui-skill` | partial | client cards; the skill row after #129 | the host keeps no tool-name table |
| Cards for runtime-defined plug-ins | none by design (ADR-0103) | — | `dsh-client-ui-cordis` | different-by-design (ADR-0103, AGENTS.md) | none wanted | tools fixed at assembly by snapshot |
| Image and file attachments in composer, messages, trajectory | none | — | `dsh-client-ui-attachment` | missing, owner 2026-10-10: plan (seam: #710) | a client feature after image input (#694) | — |
| Timing ledger of one run with an interactive overview | `RequestTiming` records (ADR-0121) | `scripts/journal-timing.py` | `dsh-client-ui-trajectory` | partial | an interactive view in a client | the timing lives in the journal; a view is a pure reader |
| Changed-files card, per-file comparison, delivery cards | none (`ObservedFiles`; the `WorkspaceMutations` counter) | `p1-tool-edit`, `p1-tool-patch`, `p1-tool-write` | `dsh-client-ui-deliverables` | missing | per-file comparison and deliverables (rank 17) | — |
| Plan mode UI (controls, cards, previews) | none | — | `dsh-client-ui-plan` | missing | with plan mode (family 3) | — |
| Goal bar above the composer | none | — | `dsh-client-ui-goal` | missing | with goals (family 3) | — |
| Approval composer (frame-wide takeover) | host `Asker` (`host/policy.rs`, host composition; ADR-0024) | ACP `session/request_permission` (ADR-0154) | `dsh-client-ui-approval` | partial | a client takeover; the ACP request is served | with no operator left, p1 denies and runs nothing unanswered |
| Questions to the user (composer takeover, plan review) | host `QuestionAsker` (`host/questions.rs`); `runtime/questions.rs` (`UserQuestionsService`) | `p1-tool-question`; ACP `elicitation/create` #674 | `dsh-client-ui-user-questions` | partial | a client composer; a plan-review intent | user-invocable only (ADR-0135) |
| Permission presets: default and per-session `/permission` | policy modules `p1-module-policy-ask`, `p1-module-policy-full-access` (ADR-0038); ACP session mode (#696) | `p1-acp` `session/set_mode`; `/access` shows the mode | `dsh-client-ui-permission-presets` | partial, owner 2026-10-10: plan (seam: #696) | a default in the environment file; a line-mode switch | — |
| Message feedback (like, dislike, dialog, `/feedback`) | none | — | `dsh-client-ui-message-feedback` | missing (owner 2026-10-10: not now; local-only ratings possible later, never uploaded) | local ratings not now | — |
| Subagent conversations (catalog, continuation routing, `@` references) | `p1-tool-delegate` tools (ADR-0050, ADR-0131) | ACP worker transcript tagging #681, worker stop #682 | `dsh-client-ui-subagent` | in-flight #681 | browse child conversations and route continuations in a client | a worker gets only the tools its parent grants |
| Workflow run node with nested member disclosure | `FrontEnd` workflow hooks (`host/frontend.rs`; ADR-0075) | ACP `_p1/workflow_update` #680 | `dsh-client-ui-workflow-run` | in-flight #680 | the ACP tree update; whether a run tree survives restart is unknown | steps arrive as typed events |
| Model choice (catalog picker, Models page, onboarding) | `--model`, `--effort`; `p1-model-profile`; switching between turns (ADR-0049) | ACP config options #675 | `dsh-client-ui-model-selection`, `dsh-client-ui-settings-models` | partial | a catalog picker and onboarding in a client (#675) | — |
| Agent presets (default, this session's, composition editor) | environment files resolved by `p1-assembly` | — | `dsh-client-ui-agent-preset` | partial | an editor and a default-for-new-sessions in a client | validated into a resolved environment before any agent is built |
| Settings framework and General page | none (files and flags) | — | `dsh-client-ui-settings`, `dsh-client-ui-settings-general` | missing | a client settings UI; ACP config options #675 cover model and effort | — |
| Account: sign in to DeepSeek, sign out, billing pages | CLI `p1 login` (ADR-0040, ADR-0044) | — | `dsh-client-ui-settings-account` | different-by-design (owner 2026-10-10: the direct DeepSeek API route only; no account sign-in or billing screens) | none wanted: `p1 login` stays the key entry | credential values masked everywhere; environment wins over the file |
| Settings pages for loop, shell, subagents and web search | environment keys (`[tool_concurrency]`; ADR-0131) | — | `dsh-client-ui-settings-agent-loop`, `-shell`, `-subagent`, `-web-search` | partial | client pages; web search absent (#516) | limits bounded in the environment file |
| Upload of session logs to the vendor API | none | `p1-journal` (local only) | `dsh-client-ui-settings-session-log` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | no upload sink exists |
| Product event collection and reporting to the host | none (`p1-usage` is a quota ledger) | — | `dsh-client-product-analytics` | different-by-design (owner 2026-10-10: never; no data leaves the machine) | none wanted | — |
| Session list (tree, search, grouping, state dots) | none: resume by path (ADR-0031) | `p1-journal` | `dsh-client-ui-sidebar` | partial, owner: after ACP door | #62 list and ACP #691, then a client tree | a session file is owned before it is read |
| Right-hand sidebar dock | none | client | `dsh-client-ui-sidebar-right` | missing, owner: after ACP door | client | — |
| Workspace file tree tab | `runtime/directory_listing.rs` (`DirectoryListingService`); `ls` (ADR-0115, ADR-0101) | `p1-tool-ls` | `dsh-client-ui-sidebar-files` | partial, owner: after ACP door | a client tree | listings are bounded and refuse oversized output |
| Document previews (Office, spreadsheets, Markdown, images, PDF) | the `read` tool, text only | `p1-tool-read` | `dsh-client-ui-sidebar-documentpreview` | missing, owner: not now | owner decision | — |
| Sandboxed web browser tabs | none | — | `dsh-client-ui-sidebar-browser` | missing, owner: not now | owner decision | — |
| Interactive shell tabs | `shell`, `shell_job` (ADR-0117, ADR-0123) | `p1-tool-shell` | `dsh-client-ui-sidebar-terminal` | partial, owner: after ACP door | needs PTY sessions (family 8) and a client | process-group kill and bounded output |
| Background job list in the session header | `runtime/jobs.rs` (`ProcessJobsService`, `JobObserver`) | `p1-tool-read-output` | `dsh-client-ui-jobs` | partial | a client list; `_p1/agent_state` #683 | job output redacted and paged by cursor |
| Open the workspace folder or a file in an installed application | none | — | `dsh-client-ui-open-in-app` | missing, owner: after ACP door | client; owner decision | — |
| Choosing the workspace folder (browse, native chooser, picker) | `--workspace DIR`; confinement always on (ADR-0025) | `runtime/` listing service | `dsh-client-ui-workspace`, `dsh-client-ui-directory-picker-browse`, `dsh-client-ui-directory-picker-native` | partial, owner: after ACP door | a client picker; ACP `session/new` carries cwd | no UI can move the file-tool root mid-run |
| Scheduled tasks and session reminders page | none | — | `dsh-client-ui-schedule` | optional, owner 2026-10-10: plan (seam: #716) | a client page over a `_p1/schedule` extension (dsh: experimental schedule bundle only) | — |

## Family 15: source-only packages: LSP, SSH, subagent backends (11 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Code navigation by language server (definition, references, implementation, hover) | none; if built: a tool module with the server process behind `ProcessService` and a host-side provider list at the root | text navigation only: `p1-tool-search`, `p1-tool-ls`, `read` | `dsh-lsp`, `dsh-lsp-stdio`, `dsh-tool-lsp` | missing | semantic navigation (rank 25); no issue | — |
| Remote workspace over SSH (files, processes, sandbox elsewhere) | the `runtime/capabilities.rs` traits (`WorkspaceService`, `ProcessService`, `SnapshotService`, `MutationService`) are the seam | native only | `dsh-ssh`, `dsh-fs-ssh`, `dsh-subprocess-ssh`, `dsh-sandbox-ssh` | missing, owner 2026-10-10: plan (seam: #713) | a `RemoteConnection` port and three remote adapters over it, no tool change | — |
| External-agent subagent backends (another harness as a worker) | `WorkerService` (`p1-workers`), one implementation | `InProcessWorkers` | `dsh-subagent-acp`, `dsh-subagent-dsh-sdk`, `dsh-subagent-claude-code`, `dsh-subagent-codex` | missing, owner 2026-10-10: plan (seam: #715) | a `backend` field per `subagents.toml` entry and one adapter crate per harness; the permission mode a closed enum, fail closed | exact grants; a leaf cannot get delegation tools; `finish` outcome host-observed (ADR-0102) |
| Programmatic client for a running harness (typed SDK) | ACP (`p1 acp`); the headless CLI | `p1-acp` | `dsh-sdk-client` | built | no in-tree client library by decision (#670 D1) | ACP has cancel and permission requests, which dsh's SDK client lacks |
| Alternative durable storage backend (SQLite) | `contracts/journal.rs` (`CommitSink`) | `p1-journal` | `dsh-storage-sqlite` | optional | no second medium; need unknown (no issue); dsh wires SQLite in no bundle | JSONL recovers a torn tail; writer lock before reading |
| Web search provider choice (Exa, Perplexity) | none (#516: host-owned backend) | — | `dsh-web-search-exa`, `dsh-web-search-perplexity` | missing, owner 2026-10-10: plan (seam: #717) | one `SearchBackend` adapter each; a normalized result type with an explicit omitted count | — |
| Session title generation (every prompt) | none | — | `dsh-session-title-all-prompts-llm` | optional | ACP titles #693 (after the first turn, again on change); dsh wires this package nowhere | — |
| Search and read earlier sessions from the model | none | — | `dsh-tool-session-query` | optional | a tool module over a host-held read service (the `tool-outputs` pattern); after #62 and #517 (dsh: opt-in model tools) | — |
| Persistent interactive terminal sessions | none (ADR-0117 excludes it) | `p1-tool-shell`, `p1-tool-shell-job` | `dsh-tool-terminal` | partial | a stateful terminal, send-input, signals, per-agent ownership (rank 24) | jobs end by notification; redacted output store; the sandbox masks credential directories |
| Build-time type-to-schema generation | WIT (`modules/wit/*.wit`) plus the JSON-schema bundle in `p1-module-protocol` | — | `dsh-typert-generator` | different-by-design (ADR-0071, ADR-0082) | whether the schema bundle is generated or checked by a test is unknown | wire values reject unknown fields |
| Developer hot reload of plug-ins | none by design (ADR-0071; nearest: ADR-0084, ADR-0078) | — | `cordis-plugin-hmr` | different-by-design (ADR-0071) | none wanted | — |

## Family 16: source-only packages: experimental, tests and apps (12 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Browser use (model drives a real browser) | none | — | `dsh-browser-use`, `dsh-experimental-browser-use-runtime`, `-playwright-mcp`, `-chrome-devtools-mcp`, `-stagehand-native` | optional, owner: not now | would be a module plus a host-granted capability (dsh: experimental, shipped switched off) | — |
| Computer use (model drives the desktop) | none | — | `dsh-computer-use`, `dsh-experimental-computer-use-cua-driver-mcp`, `-native` | optional, owner: not now | whole capability; Linux only today (dsh: experimental, shipped switched off) | — |
| Programmatic tool calling in Python | none | rhai workflows differ (ADR-0053, ADR-0114) | `dsh-experimental-ptc-runtime-python` | optional, owner 2026-10-10: plan (seam: #714) | a second runtime adapter behind the same port (dsh: experimental, mounted by no shipped profile) | the script engine is in-process rhai under caps, not an unsandboxed subprocess |
| Browser-hosted preview of the harness | none by decision (ADR-0071) | — | `dsh-experimental-webworker-runtime`, `dsh-experimental-webworker-packer` | different-by-design (ADR-0071) | none wanted | kernel namespaces for the shell, not a logical virtual filesystem |
| Live runtime inspection (DevTools view) | the journal (ADR-0021, ADR-0121) | — | `dsh-experimental-inspector` | optional | an ACP client feature; no issue (dsh: experimental opt-in bundle) | post-hoc inspection without a debug port |
| Web client boot and docking layout | none; the ACP door | `p1-acp` | `dsh-client-web`, `dsh-client-ui-dockkit` | different-by-design (#670 D1) | none wanted | one protocol for every screen |
| Desktop application (Electron shell and host process) | none; release channel (ADR-0065) | — | `dsh-desktop`, `dsh-desktop-host` | different-by-design (#670: a GUI is deferred behind ACP) | a GUI, when decided, as an ACP client | a verified release channel; the binary names its commit |
| Scripted model endpoint with fault injection | `ScriptedTransport` (`p1-provider-http::testing`) | `p1-provider-conformance` | `dsh-llm-mock-server` | partial | seeded fault mixing; a loopback endpoint for the real binary (ADR-0089 allows it) | deterministic faults, no sockets or ports |
| Recorded-session replay and keyless snapshot tests | `ScriptedProvider` (`p1-testkit`) | — | `dsh-llm-replay`, `dsh-session-snapshot` | partial | journal-to-script derivation and a snapshot refresh mode; ACP NDJSON fixtures planned (#670) | — |
| Agent-loop test harness | `p1-testkit` fakes for every contract | — | `dsh-agent-loop-testkit` | built | none | no clocks or I/O in the fakes; failure injection at an exact journal sequence |
| Whole-product boot smoke test | `p1-host/tests/cli.rs` and siblings spawn the binary | `p1-module-tests` | `dsh-loader-smoke` | partial | a reusable boot-and-run-one-turn helper | — |
| Client-side test doubles (Remote mock, client slot runtime) | none (no Remote, no slots); ACP fixture replay over a duplex pipe planned | — | `dsh-remote-mock`, `dsh-client-test-runtime` | different-by-design (#670 design choice 10) | none wanted | — |

## Planned seams (owner 2026-10-10)

The owner marked these capabilities "plan": "Plan it does not necessarily mean build it; it can
mean plan it so we can easily implement it later." Each entry designs the seam now (the port, the
interface shape, the dsh packages to match, what a later adapter supplies) so that implementing it
later is a new adapter, not a rewrite. The sketches are shapes, not code: no trait or interface
shown here exists in the repository yet, and a NEW port is added in the slice that builds it
(ADR-0153 Decision 9), in one of the two port forms of Decision 1: a trait in `p1-contracts`
where native code consumes it, or a WIT interface under `modules/wit/` with its host service
trait in `p1-module-runtime` where a module consumes it ("host-side" below means that form,
until the slice picks). One unassigned seam-design issue carries each entry; the two that already
had an issue (#695, #696) carry the design as a comment. No implementation is ordered.

### 1. OS adapters: sandbox runners and processes on Linux, macOS and Windows (seam issue #709)

Rows: family 2 "Local sandbox launcher: Landlock fallback beside bubblewrap"; family 6 "PowerShell
one-shot shell"; family 8 "Platform sandbox runners (bwrap, Landlock, Seatbelt)" and "Subprocess
service". Port: NEW `SandboxRunner` (host-side; today the concrete `Sandbox` struct and
`bwrap_args` in `runtime/process/sandbox.rs` are the only runner). `ProcessService`
(`runtime/capabilities.rs`) stays the process port and gains one adapter per OS. The shell stays a
tool module over the `process` capability; PowerShell is a second tool module, not a flag.

```rust
pub enum Enforcement { Full, Partial }
pub struct Confined { pub argv: Vec<String>, pub enforcement: Enforcement, pub denial_signatures: Vec<String> }
pub trait SandboxRunner: Send + Sync {
    fn probe(&self) -> BoxFuture<'_, Probe>;      // Usable(Enforcement) | Unusable(reason); once per host, cached
    fn confine(&self, argv: Vec<String>, policy: &SandboxPolicy /* #696 seam */)
        -> BoxFuture<'_, Result<Confined, SandboxUnavailable>>;   // fail closed
}
```

The root probes runners in a fixed order per platform (Linux: bubblewrap, then Landlock; macOS:
Seatbelt; Windows: restricted token plus ACL write grants) and keeps the first usable one; with
none, no command runs unless the session mode is the unsandboxed one (#696). `ProcessService`
adapters own group lifetime (Linux: transient user scope or process group; macOS: process group;
Windows: kill-on-close job). `CREDENTIAL_DIRECTORIES` stay refused on every platform. dsh to match:
`dsh-sandbox` (port) / `dsh-sandbox-policy` / `dsh-sandbox-local` (bwrap, Landlock, Seatbelt
profiles; probe once, fail closed) / `dsh-sandbox-windows-acl` / `node-addon-system`;
`dsh-subprocess` / `dsh-subprocess-local`; the `dsh-shell` executor seam (`dsh-bash-local`,
`dsh-bash-sandbox`, `dsh-pwsh-local`, `dsh-pwsh-sandbox`) / `dsh-tool-pwsh`. A later adapter
supplies: one `SandboxRunner` per platform with its probe and profile builder; one `ProcessService`
per OS; a `p1-tool-pwsh` module; a CI runner on that OS (the gate runs on `ubuntu-latest` only).
Keep: the PID namespace kills every descendant; credential masks follow workspace mounts; the
request type stays closed (no env, cwd or sandbox per call).

### 2. Image and file input (seam issue #710)

Rows: family 4 "Image offload", "File attachments (any file, stored verbatim)", "Image attachments
(validate, normalize, per-route versions)"; family 5 "Model catalog, discovery and per-model
capabilities" (the modality flag); family 6 "Image reading"; family 14 "Raw file upload into an
agent's attachment area" and "Image and file attachments in composer, messages, trajectory" (client
part, with #694). Port: NEW `AttachmentStore` (host-side) with its reference types in
`p1-contracts`; `contracts/history.rs` `Item::User` grows from text to content blocks;
`contracts/provider.rs` `RouteDescription` gains `input_modalities`; `read_image` is a tool over a
new `attachments` WIT capability; offload is a `ContextPolicy` reaction to a provider error.

```rust
pub enum UserBlock { Text(String), Image(ImageRef), File(FileRef) }   // references in history and journal, never bytes
pub struct ImageRef { pub id: AttachmentId, pub media_type: ImageType, pub bytes: u64, pub width: u32, pub height: u32 }
pub struct FileRef { pub id: AttachmentId, pub name: String, pub bytes: u64 }
pub trait AttachmentStore: Send + Sync {
    fn limits(&self) -> ImageLimits;
    fn save_image(&self, bytes: Vec<u8>, name: Option<String>) -> BoxFuture<'_, Result<ImageRef, AttachmentError>>;  // validate, normalize, strip metadata
    fn read_image(&self, r: &ImageRef, target: Option<ImageTarget>) -> BoxFuture<'_, Result<Vec<u8>, AttachmentError>>; // re-verified; per-route size variant
    fn save_file(&self, bytes: Vec<u8>, name: String) -> BoxFuture<'_, Result<FileRef, AttachmentError>>;
    fn file_path(&self, r: &FileRef) -> PathBuf;   // a read-only path the model's file tools can read
}
```

A provider adapter projects `Image` blocks into its wire (Messages: an image block; the direct
DeepSeek route: a Files API upload) and `File` blocks into one text line naming the read-only path;
a route whose model lacks `image` input gets text placeholders instead. A provider error that names
images (NEW: a `ProviderErrorKind::ImageOffloadRequired` variant and a provider-error input on
`ContextInput`) lets the context policy replace the oldest images
with placeholders, journalled as `ContextReplaced`. dsh to match: `dsh-attachment` (port) /
`dsh-attachment-local` (content-addressed store, normalization, per-route variants) /
`dsh-compaction-image-offload` / `dsh-tool-fs` (`read_image`) / `dsh-llm` (`inputModalities`). A
later adapter supplies: the local store (pinned directory, exchange writes, 0400 objects, never
deleted; ADR-0108, ADR-0111), the wire mapping in each provider crate, the `image` flag per model
profile, the ACP mapping (#694). Keep: credential redaction before anything is stored.

### 3. Hooks: a core plus a Claude Code bridge (seam issue #711)

Rows: family 9 "Hook protocol core (matcher, codec, merge, `hook/*` events)", "Claude Code hook
compatibility bridge", "Codex hook compatibility bridge" (the Codex bridge is optional). Port: NEW
`HookRunner` contract naming the interception points p1 offers; the two that exist carry the first
events (`AuthorizationPolicy` before a tool call, `ContextPolicy` before a request). The core is a
native crate composed at the root as a wrapper around the assembled policies (the `RedactingTool`
pattern), because a policy module in WebAssembly holds no `process` capability. Each bridge is an
adapter over the core that reads one harness's hooks file.

```rust
pub enum HookEvent { SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, SubagentStart, SubagentStop }
pub struct HookCommand { pub matcher: Matcher /* literal alternatives or regex */, pub command: String, pub timeout: Duration }
pub struct HookOutput { pub decision: Option<HookDecision /* Allow | Ask | Deny { reason } */>, pub additional_context: Option<String>, pub stop: bool }
pub trait HookRunner: Send + Sync {   // match, run through ProcessService with a cleared environment, decode stdout JSON, merge
    fn run(&self, event: HookEvent, payload: serde_json::Value, cancel: CancellationToken) -> BoxFuture<'_, HookOutput>;
}   // merge: deny over ask over allow; the first stop sticks; context accumulates in file order
```

Exit 2 blocks with stderr as the reason; any other failure is logged and does not block.
`PreToolUse` maps onto `AuthorizationPolicy::authorize` (deny, or ask through the host's `Asker`);
`UserPromptSubmit` and `SessionStart` context onto `ContextPolicy`; `Stop` onto the bounded
continuation (ADR-0037). `PostToolUse` feedback and `updatedInput` are new interception points and
wait for the deliberate contract change `policy.rs` names, made when the bridge is built. A hook
payload never carries a credential; a hook can narrow a grant, never widen one. dsh to match:
`dsh-hook-protocol` (matcher, runner, codec, merge, events) / `dsh-hooks-claude-code` /
`dsh-hooks-codex`. A later adapter supplies: the hooks-file reader and event mapping per harness
(Claude Code: the seven events dsh supports; Codex: five, block only), `hook/invoked` and
`hook/result` journal records, and the environment key that switches a bridge on. Keep:
`p1-hook-shadow` stays detached and fail-open (ADR-0058); hook text reaches the model labelled.

### 4. The direct DeepSeek API route (seam issue #712)

Rows: family 5 "DeepSeek Messages wire adapter (direct official API)". Account sign-in, balance,
DeepSeek search and log upload are different-by-design (owner 2026-10-10). Port: the existing
`Provider` port (`contracts/provider.rs`); no new port. What varies is the Messages dialect (a
closed enum in `p1-provider-anthropic`, today `ClaudeCodeSubscription | OpencodeGo | Zai`), the
route file and the account file; dsh's wire for this route is Anthropic-Messages-like, which the
Messages adapter already speaks for DeepSeek models through OpenCode Go (ADR-0134, ADR-0138).

```toml
# routes/deepseek-official.toml (sketch)
adapter = "anthropic-messages"
endpoint = "https://api.deepseek.com/anthropic"     # the adapter appends /v1/messages; origin compiled in (ADR-0110)
account = "deepseek-official"
[adapter_settings]
dialect = "deepseek-official"       # x-api-key header, effort as output_config.effort, no Claude identity, no betas
[models."deepseek-v4.1-flash"]
wire_model = "<official id: unknown>"
# accounts/deepseek-official.toml: origins = ["https://api.deepseek.com"], [credential] method = "api-key", env = "DEEPSEEK_API_KEY"
```

`p1 login deepseek-official` stores the key by reference (ADR-0108, ADR-0139); the credential can
reach no other origin. Image upload through the Files API belongs to the image seam (#710).
dsh to match: `dsh-llm` (port) / `dsh-llm-deepseek` (wire adapter without a route of its own) /
`dsh-llm-deepseek-api-key` (route plus credential); `dsh-llm-deepseek-account` and
`dsh-deepseek-account` are out by owner decision. A later adapter supplies: the dialect arm, the
route and account files, one model profile per official id, and conformance fixtures in
`p1-provider-conformance`. Unknown: whether the OpenCode Go dialect fits unchanged (it adds an
`x-opencode-session` header); the official wire model ids. Keep: a route cannot send its credential
elsewhere; diagnostics never carry a credential or a body.

### 5. SSH remote execution (seam issue #713)

Rows: family 8 "File access service (resolve, read, list, atomic write, edit)"; family 15 "Remote
workspace over SSH (files, processes, sandbox elsewhere)". Port: the runtime capability traits
`WorkspaceService`, `SnapshotService`, `MutationService` and `ProcessService`
(`runtime/capabilities.rs`) plus the `SandboxRunner` of seam 1 are the seam; the remote adapter is
now a decided second adapter, so they move to `p1-contracts` when it is built (ADR-0153 Decision
8). NEW `RemoteConnection` port (host-side), so the three service adapters do not own the transport.

```rust
pub trait RemoteConnection: Send + Sync {   // one OpenSSH alias; one digest-pinned helper on the remote host
    fn request(&self, method: &str, params: serde_json::Value, cancel: CancellationToken)
        -> BoxFuture<'_, Result<serde_json::Value, RemoteError>>;
    fn open_stream(&self, path: &str, cancel: CancellationToken)
        -> BoxFuture<'_, Result<Box<dyn RemoteStream>, RemoteError>>;   // one per process stream
}
// adapters over it: RemoteWorkspace (WorkspaceService + SnapshotService + MutationService: fs.*),
// RemoteProcessService (ProcessService: process.prepare/start/done/terminate),
// RemoteSandboxRunner (SandboxRunner: sandbox.confine on the remote host, fail closed)
```

The backend is chosen once per session at the root (`settings.toml [workspace] backend = "local" |
"ssh:<alias>"`, or the ACP session's `_meta`), never per call; the three remote adapters are
paired on one connection. Read-before-mutate (`ObservedFiles`, ADR-0025) and the write gate sit
above the port, so a remote backend inherits them; tools change nothing. dsh to match: `dsh-ssh`
(connection) / `dsh-fs` (port) with `dsh-fs-ssh` / `dsh-subprocess` (port) with
`dsh-subprocess-ssh` / `dsh-sandbox` (port) with `dsh-sandbox-ssh`. A later adapter supplies: the
helper binary and its digest pin, strict host-key checking, per-stream authentication, the error
mapping onto `FsError`, a read cap, and tests over a fake connection (no network). Keep: leaf
replacement by exchange; one `WriteGate` for all agents (ADR-0032); credential files unreadable.

### 6. Programmatic tool calling (seam issue #714)

Rows: family 3 "Tool presentation per agent (native schemas vs PTC)"; family 6 "Programmatic tool
calling (`run_code`)"; family 7 "PTC runtime (model-written programs that call tools)"; family 16
"Programmatic tool calling in Python". Port: NEW `ProgramRuntime` (host-side; a `program-runtime`
WIT capability for the `run_code` tool module). The tool executor stays the agent's scheduler:
every nested call passes `AuthorizationPolicy::authorize`, the concurrency rules and the journal
like a native call. Presentation is an assembly-time environment key, not a runtime service.

```rust
pub struct Program { pub language: Language, pub source: String, pub timeout: Duration }
pub trait ToolBinding: Send + Sync {   // the host-side closure into the scheduler; never run_code itself
    fn call(&self, name: &str, args: serde_json::Value, cancel: CancellationToken) -> BoxFuture<'_, Result<serde_json::Value, BindingError>>;
}
pub struct RunResult { pub value: Option<serde_json::Value>, pub logs: String, pub failure: Option<RunFailure /* Exception | Timeout | Abort | OutputLimit | Protocol */> }
pub trait ProgramRuntime: Send + Sync {
    fn language(&self) -> Language;
    fn instructions(&self) -> &str;   // the model-facing SDK text for that language
    fn run(&self, program: Program, tools: Arc<dyn ToolBinding>, cancel: CancellationToken) -> BoxFuture<'_, RunResult>;
}
// environment.toml: [tools] presentation = "native" | "ptc" | "both"; "ptc" with no runtime fails at assemble
```

Only the outer result enters history; each nested call's output goes to the output store
(ADR-0109) under its own call id `<parent>:ptc:<n>`. The bindings are the agent's assembled tools,
re-resolved per call. dsh to match: `dsh-agent-tool-presentation` / `dsh-tools` (`run_code`) /
`dsh-ptc-runtime` (port) / `dsh-ptc-runtime-node` / `dsh-experimental-ptc-runtime-python`. A later
adapter supplies: one runtime per language (preferred: a guest program inside the wasmtime module
runtime under ADR-0112 deadlines and the ADR-0092 hostcall budget, not a subprocess), its SDK text, reserved
names and output caps. Keep: no unsandboxed interpreter; the rhai workflow engine (ADR-0053) stays
a separate seam: scripts call workers, programs call tools.

### 7. External-harness workers (seam issue #715)

Rows: family 12 "Start a child agent and get its result (one-shot delegation)"; family 15
"External-agent subagent backends (another harness as a worker)". Agent teams: not now. Port:
`WorkerService` (`p1-workers/src/lib.rs`: start, status, wait, cancel, continue_child, list,
describe) moves to `p1-contracts` when the second backend is built (ADR-0153 Decision 8; now
decided). The variation is per subagent definition, not per service: `environments/subagents.toml`
gains a `backend` field, and the one `WorkerService` built at the root routes each start to the
backend its definition names, through a NEW `WorkerBackend` port.

```toml
[[subagents]]
subagent_type = "cc-reviewer"
backend = "claude-code"           # default "in-process"; "acp", "claude-code", "codex"
[subagents.backend_settings]
command = "claude"                # per backend: a closed permission enum, default deny; no host-CLI fallback
permission = "dontAsk"
```

```rust
pub trait WorkerBackend: Send + Sync {   // the in-process one is today's InProcessWorkers
    fn start(&self, spec: ChildSpec, cancel: CancellationToken) -> BoxFuture<'_, Result<Box<dyn ChildRun>, WorkerError>>;   // Ok only once a real child exists
}
pub trait ChildRun: Send {
    fn wait(&mut self) -> BoxFuture<'_, ChildResult>;   // final_text, turn_end, usage: None, report
    fn cancel(&mut self) -> BoxFuture<'_, ()>;
}
```

An external backend spawns through `ProcessService` (cleared environment, process group), speaks
ACP to the child (initialize, session/new, session/prompt; the stop reason maps onto `TurnEnd`),
returns final text only, denies every permission request (fail closed), and answers
`continue_child` with unsupported. `ChildSpec.tools` cannot reach a foreign harness: a definition
with a non-empty `tools` list and an external backend fails at assembly. Returned text passes
`p1-redact` before it enters the parent's history. dsh to match: `dsh-subagent` (port) /
`dsh-subagent-spawn-in-process` with `dsh-subagent-in-process-driver` / `dsh-subagent-acp` /
`dsh-subagent-claude-code` / `dsh-subagent-codex` / `dsh-subagent-dsh-sdk` / `dsh-tool-subagent`. A
later adapter supplies: one backend crate per harness (`p1-workers-acp` first; Claude Code and
Codex as pinned-protocol variants), its permission enum and stop-reason mapping, a fake-harness
test. Keep: exact grants; a leaf never gets delegation tools; the outcome is host-observed (ADR-0102).

### 8. Scheduled runs and webhook-started runs (seam issue #716)

Rows: family 12 "Scheduled reminders (once, interval, daily, weekly, cron)" and "Inbound webhook
that starts a new agent session"; family 14 "Scheduled tasks and session reminders page" (client
part); family 3 "Current time context" stays optional. Port: NEW `Schedule` port (records plus a
store with one owner) and NEW `TriggerRule` port that turns a verified external event into a
session request. Both deliver through the front-end port of ADR-0152: `SessionHandle::prompt` for
an existing session, a headless run for a new one. No listener lives in `p1-host`; an ingress
adapter is its own crate, and nothing listens unless the environment names it.

```rust
pub enum When { After(Duration), At(UtcMs), Every(Duration), Daily { at: LocalTime }, Weekly { day: Weekday, at: LocalTime }, Cron(String) }
pub struct ScheduleRecord { pub id: ScheduleId, pub title: String, pub prompt: String, pub session: Option<SessionId>,
                            pub when: When, pub active: bool, pub last_delivery: Option<Delivery> }
pub trait Schedule: Send + Sync {   // tools schedule_create/list/update/delete over a `schedule` WIT capability
    fn create(&self, r: ScheduleRecord) -> BoxFuture<'_, Result<ScheduleId, ScheduleError>>;
    fn list(&self) -> BoxFuture<'_, Vec<ScheduleRecord>>;
    fn update(&self, expected: &ScheduleRecord, new: ScheduleRecord) -> BoxFuture<'_, Result<(), ScheduleError>>;   // conflict on mismatch
    fn delete(&self, id: &ScheduleId) -> BoxFuture<'_, Result<(), ScheduleError>>;
}
pub struct VerifiedDelivery { pub provider: String, pub source: String, pub delivery_id: String, pub payload: serde_json::Value, pub received: UtcMs }
pub struct SessionRequest { pub workspace: PathBuf, pub title: String, pub prompt: String, pub environment: String, pub access: AccessPreset }
pub trait TriggerRule: Send + Sync { fn run(&self, d: &VerifiedDelivery, cancel: CancellationToken) -> BoxFuture<'_, Option<SessionRequest>>; }
```

A timer driver (one per host process) fires due records and starts a headless run or prompts the
named session. An ingress adapter (`p1-webhook-github`: one exact path, POST JSON only, HMAC with
a credential reference per ADR-0108, 202 before any rule runs) verifies, then hands a
`VerifiedDelivery` to the rules. At most once; no retry queue (as dsh). dsh to match:
`dsh-schedule` (service, tools, record) / `dsh-storage-domain` (store) /
`dsh-experimental-schedule-bundle`; `dsh-webhook` (rules, session request) / `dsh-webhook-github`
(ingress) / `dsh-host-webserver`. A later adapter supplies: the store file (one owner, atomic
replace), the timer driver, the GitHub ingress, a `_p1/schedule` ACP extension for a client page.
Keep: unknown is printed as unknown; a run started from outside gets the same access preset rules.

### 9. Web search and fetch (seam issue #717)

Rows: family 7 "Web access service (search and fetch behind one provider seam)", "Anonymous public
URL fetch with SSRF protection", "Web search backend (native DeepSeek search)", "Model-facing web
tools (`web_search`, `web_fetch`)"; family 15 "Web search provider choice (Exa, Perplexity)".
Issue #516 keeps its definition of done. Port: NEW WIT capability `web` for the tool class
(`modules/wit/web.wit`), and a host-side `WebService` with two inner ports, `SearchBackend` and
`FetchBackend`, chosen at the root; tools get neither `http` nor `credential-control`.

```wit
interface web {
  record search-request { query: string, max-results: u8, include-domains: list<string>, exclude-domains: list<string>, recency: option<recency>, country: option<string> }
  record source { url: string, title: option<string>, snippet: option<string>, published: option<string> }
  record search-result { answer: option<string>, sources: list<source>, omitted: u32 }
  record fetch-result { final-url: string, status: u16, body: body, truncated: bool }   // body: html | text
  search: func(req: search-request) -> result<search-result, web-error>;
  fetch: func(url: string) -> result<fetch-result, web-error>;
}
```

```rust
pub trait SearchBackend: Send + Sync { fn id(&self) -> &str; fn search(&self, r: SearchRequest, cancel: CancellationToken) -> BoxFuture<'_, Result<SearchResult, WebError>>; }
pub trait FetchBackend: Send + Sync { fn fetch(&self, url: Url, cancel: CancellationToken) -> BoxFuture<'_, Result<FetchResult, WebError>>; }
// public addresses only, every DNS answer and redirect checked, connection pinned, same-origin redirects, byte and time caps
```

`settings.toml [web] search = "<backend id>"`, `fetch = "http"`; a backend's key is an
origin-bound credential (`p1-auth`, ADR-0110). Tools: `p1-module-web-search`, `p1-module-web-fetch`
(HTML to text, every result labelled untrusted, the full text into the output store before
excerpting). dsh to match: `dsh-web` (port) / `dsh-web-fetch-http` / `dsh-web-search-deepseek`,
`dsh-web-search-exa`, `dsh-web-search-perplexity` / `dsh-tool-web`. A later adapter supplies: the
fetch backend with offline network-policy fixtures, one search backend (a DeepSeek search adapter
is not ordered: owner 2026-10-10, the direct API route only), the two tool modules, the quality
measurement of #516. Keep: a tool reaches no arbitrary URL on its own (ADR-0130); web access can be
withheld per role through grants (ADR-0050).

### 10. MCP client (seam design on #695)

Rows: family 7 "MCP client bridge (external tool servers)" and "MCP resource discovery and
reading"; the ACP part is #695. Port: the `Tool` contract for every served tool, plus a NEW
host-side `McpClient` port with one native adapter (`p1-mcp-client`: stdio and streamable HTTP; a
module can open neither a process nor a socket).

```rust
pub enum Transport { Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf> },
                     Http { url: Url, headers: BTreeMap<String, String> } }
pub trait McpClient: Send + Sync {
    fn connect(&self, name: &str, t: Transport, cancel: CancellationToken) -> BoxFuture<'_, Result<Box<dyn McpServer>, McpError>>;
}
pub trait McpServer: Send + Sync {
    fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>>;          // raw name, description, input schema, read-only hint
    fn call(&self, raw_name: &str, args: serde_json::Value, cancel: CancellationToken) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>>;
    fn resources(&self, cursor: Option<String>) -> BoxFuture<'_, Result<ResourcePage, McpError>>;
    fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>>;
}
```

Generation: at session start the host discovers each server's tools once and builds one native
`Tool` per tool, named `mcp__<server>__<raw>` with `ToolIdentity { implementation: "mcp:<server>",
variant: <raw name> }`; a generation is swapped whole, a name clash fails the swap, and a failed
refresh keeps the previous generation. `Effect` is `Executes` unless the server marks the tool
read-only; approval follows `AuthorizationPolicy` as for any tool; results are redacted and bounded
through the output store. Servers are named per environment (`[[mcp_servers]]`, host-validated)
or per ACP session (`mcpServers`, #695); a guest never names a server, and the survey's open
question is answered so: the environment names the server, never its tools. Resources are a tool
module `p1-module-mcp-resources` over an `mcp-resources` WIT capability. dsh to match:
`dsh-mcp-client` (connection, tool generations) / `dsh-mcp-resources`. A later adapter supplies:
the client crate (an MCP SDK or own JSON-RPC), the reconnect policy, the child-environment scrub,
a fake-server test with no network. Keep: tools fixed at assembly by snapshot (ADR-0103) for every
non-MCP tool; an MCP generation is the one deliberate exception and is journalled.

### 11. Sandbox modes, escalation and permission presets (seam design on #696)

Rows: family 6 "One-shot shell with background jobs" (model-requested escalation); family 8
"Process sandbox: modes, per-call policy, fail-closed, escalation" and "File write fence and
workspace confinement" (the read-only mode); family 9 "Permission presets (one selector over
sandbox plus approval)"; family 14 "Permission presets: default and per-session `/permission`".
Port: NEW `SandboxModes` (resolves the policy for one call: an approved escalation, else the
session mode, else the environment default); the `SandboxRunner` of seam 1 takes the resolved
policy; presets are a closed list in the environment file; the session mode is a journal record so
resume keeps it.

```rust
pub enum SandboxMode { ReadOnly, WorkspaceWrite, DangerFullAccess }   // replaces the two-value `SandboxMode` flag in `host/cli.rs`; the last runs no runner at all: the owner's "dangerously skip permissions"
pub struct SandboxPolicy { pub mode: SandboxMode, pub workspace_root: PathBuf }
pub trait SandboxModes: Send + Sync {
    fn resolve(&self, session: &SessionId, requested: Option<SandboxMode>) -> SandboxPolicy;   // a wider `requested` needs an approval first
    fn set_session_mode(&self, session: &SessionId, mode: SandboxMode) -> BoxFuture<'_, ()>;   // journalled as SandboxModeChanged
}
// shell call: sandbox_permissions: Option<SandboxMode>, justification: Option<String>
//   -> AuthorizationRequest { effect: Executes, escalation: Some(mode) } -> Permit | Deny | Ask (the user approves this one call)
// shell result facts: sandbox { mode, enforcement, denied: Vec<String> } so the model sees why a write failed
```

Presets (`environment.toml [access] preset = "read-only" | "workspace-write" | "full-access"`)
bundle a sandbox mode with an approval policy module (`ask` or `full-access`); `session/set_mode`
(#696) selects one of these names and nothing else, and `current_mode_update` reports it.
`ReadOnly` also makes `MutationService::begin` refuse for the session, so file tools and the shell
agree. Confinement (ADR-0025) and credential refusal cannot be weakened by any mode or preset. dsh
to match: `dsh-sandbox` (escalation vocabulary) / `dsh-sandbox-policy` / `dsh-bash-sandbox` /
`dsh-permission-presets` / `dsh-user-approval`. A later adapter supplies: the resolver, the two
escalation fields on the shell tool, denial classification per runner, the ACP mode mapping. Keep:
home hidden behind a tmpfs; credential directories refused; `Always` grants keyed to verified
digests and void on replacement.

## Feature gaps (not floor violations)

A feature gap is something dsh's default product does and p1 does not. It is not a floor
violation: the floor is modularity (owner 2026-10-10; "How to use this" above). Recounted by
script after the owner's scope answers of 2026-10-10: 66 `missing`, 59 `partial` and 7
`in-flight` rows carry a feature gap, 23 rows are `optional` (outside the floor, left out of the
ranking), 41 are `different-by-design` and 31 are `built`; of the `built` rows, 21 list remaining
feature differences in their Gap cell; 37 rows are planned seams ("Planned seams" above). The
survey ranked 30 gaps, 29 listed below (rank 20 left the list), by what a p1 user loses, in
this order of weight (accepted by the owner 2026-10-10): safety, correctness of their work,
ability to finish tasks, recovery from failure, cost. The ranking is the survey's judgement
(2026-10-10); the order of work is the lead's. Rank 20 (hook protocol core) left the list
after the rework: dsh ships no hook bridge in a bundle, so the row is `optional`; the other
rank numbers are kept as the survey gave them. Evidence per row is in the survey's "Top gaps
below the dsh floor" table (the survey's name for it; it predates the owner's reframing).

| Rank | Port (family) | Status | Gap in plain words | What a user loses | Owner |
|---|---|---|---|---|---|
| 1 | Overflow recovery (4) | missing | when the provider says the request is too long, p1 ends the turn; dsh compacts and retries once | recovery: a long task stops mid-way | no issue |
| 2 | Workspace instruction files (3) | built | closed by #700 (ADR-0151): discovery from the git root, a global default and a byte budget; a journal record is unknown | correctness: project rules do not reach the model | #129 (closed) |
| 3 | Landlock fallback beside bubblewrap (2) | partial | one sandbox backend; where user namespaces are blocked the only option is no confinement | safety | #709 (seam) |
| 4 | Process sandbox modes, per-call policy, escalation (8) | partial | no read-only mode, no per-session mode, no approved escalation, no denial facts | safety and finishing work | #696 (ACP modes; seam design attached) |
| 5 | Model-facing web tools (7) | missing | no web search and no web fetch | finishing tasks: no current documentation | #516 (deferred); #717 (seam) |
| 6 | MCP client bridge (7) | missing | p1 cannot use external tool servers | finishing tasks | #695 (seam design attached) |
| 7 | Request retry policy and executor (5) | partial | a stream that fails after output has started cannot be re-run at the step | recovery | no issue |
| 8 | Image attachments (4) | missing | no image input at all | finishing tasks: no screenshots | #694 (ACP part); #710 (seam) |
| 9 | ACP agent door (13) | in-flight #670 | `p1 acp` serves ACP v1 (ADR-0154, ADR-0156); load, resume, titles, images, MCP servers and modes remain | finishing tasks: editor features behind the remaining children | #691 to #696 |
| 10 | Task list tool (6) | missing | no task list the model and user both see | finishing tasks | #692 needs a source |
| 11 | Subprocess service (8) | partial | unsandboxed processes that leave the group survive | safety and cost | #709 (seam) |
| 12 | Tool output retention (6) | partial | only shell output is recoverable; other oversized results are cut | correctness | no issue |
| 13 | Skill catalog and loading (4) | built | closed by #700 (ADR-0151): configured roots, the `skill` tool; no `/name` gesture yet (see the invocation row) | finishing tasks | #129 (closed) |
| 14 | Human-guided credential flows (5) | partial | no browser sign-in or code entry for any route | ability to start | no issue |
| 15 | Outbound HTTP proxy policy (1) | missing | behaviour behind a required proxy is undefined, no diagnostics | ability to work | no issue |
| 16 | Plan mode (3) | missing | no plan-first mode with a reviewed exit | safety and correctness | no issue |
| 17 | Per-turn workspace change summary (9) | missing | no list of changed files or per-file comparison | correctness: review is harder | no issue |
| 18 | Repeated identical tool call reminder (3) | missing | a looping model is not nudged | cost | no issue |
| 19 | Context compaction (4) | partial | absolute threshold per environment; summary on the agent's own model; no range compaction | cost | no issue |
| 21 | Mid-turn input queue (3) | partial | queued messages cannot be edited, removed or cleared; not journalled before delivery | correctness and recovery | no issue |
| 22 | Continue, steer, interrupt, list children (12) | partial | running children cannot be steered or messaged; they do not survive a restart | recovery | no issue (#679 is main-turn steering) |
| 23 | Session listing, search, lineage (10) | missing | finding a past session needs its file path | recovery | #62 (epic #72); #691 |
| 24 | Persistent PTY terminal sessions (8) | missing | interactive programs and debuggers cannot be driven | finishing tasks | no issue |
| 25 | Language-server navigation (15) | missing | symbols are found by text search only | correctness | no issue |
| 26 | Permission presets (9) | partial | ACP sessions switch `ask` / `read-only` / `full-access` (#696); the sandbox and the line mode still need a restart with other flags | safety | #696 |
| 27 | Startup validation and failure policy (1) | partial | no panic reporter, no saved startup diagnostics | recovery | no issue |
| 28 | Persisted long-running goal (3) | missing | an objective spanning many turns is neither tracked nor continued | finishing tasks | no issue |
| 29 | Asking the user a question (6) | partial | questions block; no late answers, no open-question list | finishing tasks | #674 (ACP part) |
| 30 | Context pressure and token measurement (3) | partial | no breakdown of what fills the context | cost | #677 (ACP usage updates) |

## Floor check

How a new or changed feature is checked against the dsh floor (owner 2026-10-10, ADR-0153
Decision 2):

1. Find the feature's row. No row: get one through the lead first.
2. Read the "dsh counterpart" cell. The first package is dsh's port (the contract); the rest
   are its adapters and consumers. Those are the seams dsh separates for this feature; the
   survey's `family-<n>-*.md` holds the detail when the cell is not enough.
3. Build at least the ports dsh separates: one port in `p1-contracts` (or a WIT interface under
   `modules/wit/`) per seam dsh keeps apart, with its adapter crates behind it, even with one
   implementation today. p1 may have fewer parts than dsh, a narrower surface or a stricter
   rule, as long as no variation point dsh protects is fused away: a source fused with its
   consumer, a reader fused with its catalog, or a composition that fixes a choice dsh leaves
   open.
4. A feature p1 does not build is not checked; a row marked `optional` or `different-by-design`
   is outside the floor.
5. The row's "Better than dsh" cell is what the change must keep.

Whether a row built before this record meets the floor was not surveyed (the survey judged
features, not seam shapes); such a row is checked the next time it is touched.

## Owner answers to the survey's questions (2026-10-10)

The survey raised 14 questions on 2026-10-10 (full text: `SURVEY.md`, "Open questions for the
owner"). The owner answered them the same day (recorded in the survey directory's
`DRAFT-NOTES.md`, "Owner scope answers 2026-10-10"); the rows carry the answers, so no session
asks them again. "Plan" means the seam is designed ("Planned seams" above, one issue each), not
built.

| Q | Topic | Answer |
|---|---|---|
| Q1 | operating systems beyond Linux | plan all three (Linux, macOS, Windows): #709 |
| Q2 | a browser or desktop GUI | after the ACP door works |
| Q3 | telemetry, feedback and session upload | never; no data leaves the machine (rows different-by-design); local-only answer ratings: not now (owner 2026-10-10) |
| Q4 | voice input | after the ACP door works |
| Q5 | remote execution over SSH | plan: #713 |
| Q6 | programmatic tool calling | plan: #714 |
| Q7 | browser use and computer use | not now |
| Q8 | other harnesses as workers; agent teams | workers: plan, #715; agent teams: not now |
| Q9 | hooks compatible with Claude Code or Codex | plan a core plus the Claude Code bridge: #711; the Codex bridge is optional |
| Q10 | DeepSeek as a first-class vendor | the direct API route only: #712; account, balance and log upload different-by-design |
| Q11 | image and file input | plan: #710 |
| Q12 | Office documents | not now |
| Q13 | scheduled and webhook-started runs | plan both: #716 |
| Q14 | sandbox escalation and a read-only mode | plan escalation with approval, a read-only mode and an unsandboxed mode: on #696 |
| (none) | web search and fetch; MCP client | plan: #717; on #695 |
