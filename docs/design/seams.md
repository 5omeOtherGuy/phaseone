# p1 seam catalog: dsh capabilities mapped to p1 ports

Draft for ADR-0153 (proposed, 2026-10-10; reworked the same day after the owner's decisions of
~13:3x). Replaces the 2026-09 "modules and seams" draft v1,
kept verbatim as `seams-v1.md`: the section references in ADR-0002, 0003, 0017, 0025, 0032,
`STATUS.md` and `docs/SLICE-REPORT.md` ("seams.md section 3/4/5/10/11") point to that file.
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
  `optional` (dsh ships it off by default or experimental; outside the floor). `owner decision
  pending (Qn)` marks a row that waits on survey question n (list at the end of this file); do
  not ask those questions again. No status says whether a row is at the modularity floor; that
  is checked per row when it is built ("Floor check" below).
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
| `ctx.web` provider registry | search and fetch backends | none yet; #516 puts backend choice, credentials and network policy in the host behind a scoped `web` interface |
| MCP tool generations | externally served tools | none yet (missing; see family 7) |
| `ctx.jobs` | background jobs | `JobRegistry` per session in `p1-module-runtime` behind the `process-jobs` WIT interface (ADR-0117) |
| `ctx.subagents` provider table | child agent backends | one `WorkerService` implementation built at the root with an injected agent factory; `environments/subagents.toml` entries (ADR-0131) |
| session store, projection registry, storage hub | durable state and derived views | one `CommitSink` built in `host/session.rs` (memory or JSONL, ADR-0021); one projection, `p1_core::resume::project`; each persistent file has one owner |
| telemetry backend slot | outbound capture | none; `EventSink` and `CommitSink` are explicit trait objects wired in `p1-host` |
| Typert registry, Remote, UI slots, client modules | typed RPC and browser extension points | none; ACP is the one wire (`p1-acp`, own versioned wire types, #670 D6) and the `FrontEnd` trait at the root becomes an ACP-neutral port (#697) |
| `dsh-scope` | per-agent shadowing | ownership: `ToolServices` built per assembled agent, grants per worker (ADR-0050), capability snapshots on the tool (ADR-0103) |
| hook protocol listeners | user scripts in the loop | none; `AuthorizationPolicy` and `ContextPolicy` are the decision points (hooks are `optional`: dsh ships no bridge in a bundle; owner question 9) |

## Family 1: kernel, composition and configuration (18 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Plug-in kernel: services, events, lifecycle cleanup | none by design: the kernel is `p1-core`; services are traits in `contracts/` passed into `AgentParts` | `p1-core`, `p1-assembly` (`Catalog`), `p1-host` (root) | `cordis` | different-by-design (ADR-0071, AGENTS.md) | none wanted: no dynamic lookup, no event bus | a missing dependency is a compile or assembly error, not a warning at settlement |
| Config-driven plug-in tree: rows, groups, includes | environment file schema (`EnvironmentToml`, `p1-assembly`) | `p1-assembly` (`load_environment`, `assemble`), `p1-host` (environment dirs, `modules.lock`) | `cordis-plugin-loader`, `cordis-plugin-group`, `cordis-plugin-include` | different-by-design (ADR-0071, ADR-0087) | none wanted: no row patch layers or runtime row edits | unknown keys rejected everywhere; cannot name a module outside the verified release |
| Product composition (which parts a product runs) | `environments/*/environment.toml` plus `settings.toml [capabilities]` (ADR-0124) | `p1-host` root; front ends over one assembly | `dsh-base` plus mode bundles (`dsh-acp-app`, `dsh-headless`, `dsh-sdk-app`, `dsh-sdk-minimal`, `dsh-web-app`) | different-by-design (modes are front ends on one assembly; #670) | no user override layer over a shipped environment (whole-file replace by name); decide if wanted | the prompt is tied to the tool set; assembly fails on a prompt naming a missing tool |
| Launcher and command line | none (host composition, `host/cli.rs`) | `p1-host` | `dsh`, `dsh-cmdline` | partial | no config-schema dump; `p1 acp` door #670/#673 | one parser, no launcher/app split |
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
| Typed host-to-client RPC and runtime schema registry | ACP wire (`p1-acp`, own `wire::v1`, #670 D6); module protocol `p1-module-protocol` plus `modules/wit/` (ADR-0082) | `p1-acp` (PR #687) | `dsh-typert-protocol`, `dsh-typert-registry`, `dsh-typert-loader` | in-flight #670 | driver #673; no runtime schema registry (not a recorded decision; not needed today) | one fixed versioned wire and one module protocol; no generation step |
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
| Shell-free native command runner and "open in desktop" | process service `Command` (`runtime/`), cleared environment | `p1-module-runtime` | `dsh-native-command` | missing, owner decision pending (Q2) | open, reveal and default-application listing; client or host ownership undecided | child environment is cleared and rebuilt from an allow-list |
| Local sandbox launcher: Landlock fallback beside bubblewrap | none: the sandbox is a concrete `Sandbox` struct (`runtime/process/sandbox.rs`), not a port | bubblewrap only (ADR-0035, ADR-0096) | `node-addon-system` (`landlock-run`) | partial, owner decision pending (Q1) | a sandbox-runner port with a Landlock rung behind the same argument builder (rank 3) | PID namespace kills every descendant; credential masks follow workspace mounts |
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
| Workspace instruction files (AGENTS.md chain) | instruction and skill types plus a source trait in `contracts/` (decided #129; ADR-0151 unmerged) | today `host/instructions.rs` (`--instructions`, #131) | `dsh-agent-instructions` | in-flight #129 | discovery from root to cwd, global default, byte budget, change notices, journal record (rank 2) | loaded paths and hashes in the journal environment record (planned) |
| System prompt assembly | prompt template with fixed placeholders (`p1-assembly`) | `p1-assembly` | `dsh-system-prompt` | partial | runtime-context sections (time, sandbox, approval, delegation); no escape for a literal `{{` | a prompt cannot name a tool the agent lacks; assembly is pure |
| Per-agent persona | `environments/<name>/prompt.md` | — | `dsh-persona` | built | no `{{model}}` placeholder | persona fixed by grant; cannot be shadowed at run time |
| Agent presets and preset roster | `environments/<name>/environment.toml`, `environments/subagents.toml` | `p1-assembly`, `p1-host` | `dsh-agent-preset-registry`, `dsh-agent-preset` | different-by-design (ADR-0071: an environment can only name host-registered modules) | a preset roster for a client (ACP config options #675) | an environment file cannot mount code |
| Tool presentation per agent (native schemas vs PTC) | none | — | `dsh-agent-tool-presentation` | optional, owner decision pending (Q6) | the PTC selector; dsh composes it only in the web-app `ptc` preset | — |
| Current time context | `{{date}}` filled at assembly; `contracts/clock.rs` | `p1-assembly` | `dsh-time-context` | optional | time of day, zone, elapsed time, refresh (dsh mounts it only in the experimental schedule bundle); the date goes stale in a long session | — |
| tmux location context | none | — | `dsh-tmux-context` | optional | pane context for an agent in tmux (dsh mounts it in no bundle); the owner's fleet tooling lives outside p1 | — |
| Context pressure and token measurement | `contracts/provider.rs` (`Usage`), `contracts/policy.rs` (`ContextInput.last_usage`); `p1-context::estimate_tokens` | `p1-context`, `p1-journal` (usage per response) | `dsh-token-meter` | partial | a shared measure with system, tools and messages breakdown and a per-turn fold (rank 30) | unknown usage is `None`, never zero (ADR-0019) |
| Repeated identical tool call reminder | none | stall guard for headless runs (ADR-0055) | `dsh-repeat-tool-reminder` | missing | same-call loop detection and the nudge (rank 18) | — |
| Plan mode | none | read-only environments; `ask_user_question` (ADR-0116, ADR-0135) | `dsh-plan-mode` | missing | toggle, planning guidance, reviewed exit, durable mode state (rank 16) | plan-only can be enforced by assembling no write tools, where dsh's is advice |
| Persisted long-running goal | none | `finish` plus bounded continuation (ADR-0037, ADR-0120) | `dsh-goal`, `dsh-tool-goal`, `dsh-command-goal`, `dsh-goal-round-driver` | missing | cross-turn objective with phases, human-gated rounds, `/goal` (rank 28) | `finish` is an observable end act; a worker result says whether it was verified |
| Slash command registry | none: a closed list built by the host | frozen TUI today; ACP `available_commands_update` #676 | `dsh-commands` | in-flight #670 | the ACP command list (#676); no plug-in point for new commands (not a recorded decision) | discoverable by any ACP client without a registry |
| Manual compaction command | `contracts/policy.rs` (`ContextPolicy::compact_now`) | `p1-context`; `--resume --compact`; TUI `/compact` (ADR-0076) | `dsh-command-compact` | built | ACP `/compact` (#676) | one policy method serves the threshold and the manual path |

## Family 4: compaction, skills and attachments (14 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Context compaction (summarize old history, keep the recent tail) | `contracts/policy.rs` (`ContextPolicy`) | `p1-context`, `modules/p1-module-context`, host `SummaryService`; `[context]` per environment | `dsh-compaction`, `dsh-compaction-basic` | partial | threshold as a share of the routed window, summarizer on another model, range compaction (rank 19) | the replacement is journalled before it is installed; an over-window summary request is refused |
| Overflow recovery (provider says context too long: compact, then retry) | `contracts/provider.rs` (`ProviderErrorKind::ContextWindowExceeded`) | none: the kind is only displayed | `dsh-compaction-basic` | missing | compact once and retry on a provider-confirmed overflow (rank 1) | — |
| Manual compaction on demand | `ContextPolicy::compact_now` | TUI and CLI doors (ADR-0076) | `dsh-compaction` (`compactNow`) | partial | the ACP door (#676); range compaction | — |
| Compaction durability and busy lock | `contracts/journal.rs` (`ContextReplaced` record) | `p1-core` (commit before install), `resume.rs` | `dsh-compaction` | built | none | no lock bracket to orphan; the journal is the single truth |
| Tool-result pruning before summarizing | `[context] trim_at_tokens` (ADR-0127, ADR-0136) | `p1-context` | `dsh-compaction-tool-result-pruner` | built | the trim marker could name the stored-output handle for `read_output` | — |
| Image offload (swap over-budget images for placeholders, retry) | none: no image content in `contracts/history.rs` | — | `dsh-compaction-image-offload` | missing, owner decision pending (Q11) | needs image input first (#694) | — |
| Skill catalog and loading (index, body on demand) | skill types plus a source trait in `contracts/` (decided #129; ADR-0151 unmerged) | planned `p1-skill-fs`, `p1-tool-skill`; today host `--skills` (#131) | `dsh-skill`, `dsh-skill-filesystem`, `dsh-tool-skill` | partial, in-flight #129 | the `skill` tool, fixed roots with priority, description cap, journal record (rank 13) | a typed source list passed at assembly; could cap bodies and report invalid skills |
| Skill invocation policy and the user's `/name` gesture | none | — | `dsh-skill`, `dsh-skill-filesystem`, `dsh-tool-skill` | missing | per-skill visibility and the `/name` gesture; extend #129 | — |
| Skill catalog refresh while running | none | index built once at launch | `dsh-skill-filesystem`, `dsh-skill`, `dsh-tool-skill` | missing | live refresh; a body hash in the journal environment record | — |
| Bundled Office-document skills | none | — | `dsh-skill-office` | missing, owner decision pending (Q12) | would ship as plain `SKILL.md` directories read by `p1-skill-fs` | — |
| `@file` mention completion | none | — | `dsh-file-reference`, `dsh-file-reference-local` | missing | clients own completion; p1 only needs to accept references (`embeddedContext` is false in #687) | — |
| File attachments (any file, stored verbatim) | none: `Item::User` is text only | — | `dsh-attachment`, `dsh-attachment-local` | missing, owner decision pending (Q11) | attachment store, prompt-side reference type, verified reads | pinned-directory and exchange writes already exist (ADR-0108, ADR-0111) |
| Image attachments (validate, normalize, per-route versions) | none: no image type in `contracts/provider.rs` or `history.rs` | — | `dsh-attachment`, `dsh-attachment-local` | missing, owner decision pending (Q11) | image input end to end (rank 8); the ACP part is #694 | — |
| Office document conversion and rendering | none | — | `dsh-office-to-pdf`, `libreoffice-kit` | missing, owner decision pending (Q12) | whole capability | — |

## Family 5: model providers and credentials (12 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Provider-neutral model call and adapter registry | `contracts/provider.rs` (`Provider`: describe, validate, stream; one terminal event, ADR-0017) | `p1-provider-anthropic`, `p1-provider-openai`, `p1-provider-openai-chat` (modules `p1-module-provider-*`), `p1-provider-http`, `p1-provider-conformance`; route `adapter` field mapped by `p1-host` | `dsh-llm` | built | one provider per agent (the summarizer uses the same one) | closed error enum; diagnostics never carry a credential or body; no dynamic registration |
| Model catalog, discovery and per-model capabilities | `p1-model-profile` (`profiles/*.toml`), route `[models]` bindings | `p1-model-profile`, `p1-host` (`p1 models`) | `dsh-llm`, `dsh-llm-deepseek`, `dsh-llm-pi-ai` | partial, owner decision pending (Q11) | no live endpoint catalog; no input-modality flag in the profile | a route serves only the profiles it names: no unknown id reaches a paid endpoint |
| DeepSeek Messages wire adapter (direct official API) | `Provider` port | OpenCode Go routes only (ADR-0134, ADR-0138) | `dsh-llm-deepseek`, `dsh-llm-deepseek-api-key`, `dsh-llm-deepseek-account` | partial, owner decision pending (Q10) | no direct `api.deepseek.com` route, no account route, no Files API | endpoint origin compiled in; a route cannot send its credential elsewhere (ADR-0110) |
| Multi-provider adapter over a vendor catalog | `Provider` port plus route files (ADR-0039) | one adapter crate per wire protocol | `dsh-llm-pi-ai` | different-by-design (ADR-0039, ADR-0110) | none wanted | no ambient credential discovery; origin checked before any credential read |
| Request retry policy and executor | `RetryPolicy` (`p1-provider-http/src/drive.rs`); route `retry_policy` presets (ADR-0137, ADR-0145) | `p1-provider-http` | `dsh-llm-retry`, `dsh-llm` | partial | step-level re-run after partial output; a retry record before the wait (rank 7) | permanent failures are never retried; a hostile Retry-After is capped |
| Provider-specific extra request fields | typed `[adapter_settings]` in the route file | `p1-host` (settings to the provider component) | `dsh-deepseek-llm-api-extensions` | different-by-design (ADR-0093: typed route settings, host-validated; AGENTS.md: no registry) | none wanted | no module can add a field to the provider body |
| Credential store and lookup (references, not values) | `p1-auth` (`resolve`, `describe`; chain per route, ADR-0040; pinned directory, ADR-0108) | `p1-auth`, `p1-redact` | `dsh-credentials`, `dsh-credentials-local` | built | no generic named-record store for modules; no `.env` file fallback (not a recorded decision) | pinned-directory writes; sources bound to compiled origins; `Debug` prints `<redacted>` |
| Credential for a route that needs none, or a proxy injects it | `[credential] kind = "none"` (ADR-0070) | `p1-auth`, `p1-provider-http` | `dsh-credentials` (undefined), `dsh-llm-pi-ai` (keyless) | built | none | explicit declaration instead of ambient discovery |
| Account separate from route (several accounts per provider) | `accounts/*.toml` (ADR-0139) | `p1-host` (`accounts.rs`) | `dsh-llm-deepseek-api-key`, `dsh-llm-deepseek-account`, `dsh-deepseek-account` | built | none | several accounts on one route |
| Account sign-in state, profile and balance | `UsageProbe` (`p1-usage`) | `p1-usage` | `dsh-deepseek-account`, `dsh-deepseek-account-platform` | missing, owner decision pending (Q10) | sign-in state, profile, balance, session-expired event | probes carry no credential; `Unsupported` is shown, never an invented number |
| Human-guided credential flows (browser PKCE sign-in, code entry) | `p1 login` (`host/login.rs`), `p1-auth` | `p1-host` | `dsh-authorization`, `dsh-deepseek-account-platform` | partial | a browser or OAuth login for any route, a flow port, a loopback callback server (rank 14; `docs/design/credentials.md` §8.4) | a key is taken from stdin only; OAuth entries never copied between tools |
| Failure classification and account-scoped failure codes | `contracts/provider.rs` (`ProviderErrorKind`) | `p1-provider-http` (`classify_status`) | `dsh-llm`, `dsh-llm-deepseek-account` | built | no sign-in-required versus token-invalid split; `Transport` not split | reset time parsed from vendor hints |

## Family 6: tool runtime and built-in tools (15 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Tool contract, registry and execution pipeline | `contracts/tool.rs` (`Tool`), `contracts/policy.rs` (`AuthorizationPolicy`) | tools as WebAssembly modules (`modules/p1-module-*`); `p1-assembly` resolves names; `p1-redact` (`RedactingTool`) | `dsh-tools` | built | no pre- or post-execute listener chain (not a recorded decision); a `deferLoading`-style marker unknown | tool code runs in a WebAssembly sandbox with capability grants; `effect()` classifies each call before it runs |
| Parallel tool calls in one response | `contracts/tool.rs` (`Concurrency`, `Tool::concurrency`, ADR-0118) | `p1-core` scheduler; `[tool_concurrency]` | `dsh-tools` | built | none for native calls | read-only shell commands can overlap; per-environment cap |
| Programmatic tool calling (`run_code`) | none | workflows differ (ADR-0053) | `dsh-tools` | optional, owner decision pending (Q6) | whole feature (dsh composes it only in the web-app `ptc` preset) | — |
| Per-call tool time limit | none generic: shell `timeout_seconds`; module deadlines (ADR-0112) | `p1-tool-shell`, `p1-module-runtime` | `dsh-tool-call-timeout-policy` | partial | a uniform budget declaration; moot while only `shell` takes a time | a hard process-group kill by the host, not a cooperative signal |
| File read, write and targeted edit | `Tool` port plus file capability services (`runtime/`, ADR-0088, ADR-0095) | `p1-tool-read`, `p1-tool-edit`, `p1-tool-write`, `p1-tool-patch` (modules `p1-module-read`, `-edit`, `-write`, `-patch`) over `p1-workspace` | `dsh-tool-fs`, `dsh-tool-str-replace-editor` | built | directory `view` and `insert` inside one editor tool (p1 has `ls` and `ToolFace` variants) | read-before-mutate always on; leaf exchange; several files or ranges per `read` |
| Image reading | none | — | `dsh-tool-fs` (`read_image`) | missing, owner decision pending (Q11) | whole feature, with image input (#694) | — |
| File and content search | `Tool` port | `p1-tool-search` (`grep`, modes `files` and `count`), `p1-tool-ls` (ADR-0115) | `dsh-tool-fs-search` | built | modification-time ordering, over-cap sampling, recoverable capped results | pagination arguments in the schema |
| One-shot shell with background jobs | `Tool` port plus `modules/wit/jobs.wit` (`process-jobs`; `runtime/` `ProcessJobsService`) | `p1-tool-shell`, `p1-tool-shell-job` (modules `p1-module-shell`, `p1-module-shell-job`) | `dsh-tool-bash`, `dsh-tool-jobs` | partial, owner decision pending (Q14) | jobs for other producers, a wake cap, a blocking wait, model-requested sandbox escalation | bubblewrap boundary with credential masks; the host-observed exit code is the only evidence |
| Persistent shell session | none (ADR-0117 excludes it) | — | `dsh-tool-bash-persistent`, `dsh-tool-pwsh-persistent` | missing | cross-call shell state; see the PTY row (rank 24) | — |
| PowerShell one-shot shell | none | — | `dsh-tool-pwsh` | missing, owner decision pending (Q1) | a Windows target is undecided | — |
| Tool output retention (spill) | `tool-outputs` capability (`runtime/` `OutputStore`, ADR-0109) | `p1-tool-read-output` (module `p1-module-read-output`), shell | `dsh-spill`, `dsh-spill-local`, `dsh-spill-policy` | partial | spill for non-shell tools, orphan cleanup, a backend port (rank 12) | redacted before it reaches disk; random handles, never paths |
| Asking the user a question | `user-questions` interface (ADR-0116); host `QuestionAsker` (`host/questions.rs`, host composition) | `p1-tool-question` (module `p1-module-question`), `runtime/` `UserQuestionsService` | `dsh-user-questions`, `dsh-tool-ask-user` | partial | timed or pending mode, late answers, an open-questions projection (rank 29); ACP `elicitation/create` #674 | workers can ask without owning the screen; the call is gated on a user invitation (ADR-0135) |
| Task list tool | none | — | `dsh-tool-todo` | missing | the tool and its journal event (rank 10); ACP `plan` updates #692 need a source | — |
| Declaring deliverable files (`present`) | none | `finish` (ADR-0037) | `dsh-tool-present` | missing | whole tool; depends on a deliverables view in a client | — |
| Bundled script-runtime paths | none | — | `dsh-tool-workspace-dependencies` | optional | only for a product that ships its own runtimes (dsh mounts it in `dsh-sdk-app` only, disabled unless an environment variable is set) | — |

## Family 7: web, MCP, workflows and programmatic tool calling (11 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Web access service (search and fetch behind one provider seam) | none; #516 plans a scoped `web` interface in `modules/capabilities.toml` with a host-owned backend | — | `dsh-web` | missing | whole capability (rank 5); #516 deferred by the owner (D27, 2026-10-05) | tools cannot reach arbitrary URLs today: every network grant is a fixed-origin host capability (ADR-0130) |
| Anonymous public URL fetch with SSRF protection | none | — | `dsh-web-fetch-http` | missing | a fetcher with public-address check, connection pinning, redirect policy and caps (#516 definition of done) | — |
| Web search backend (native DeepSeek search) | none | origin-bound credentials in `p1-auth` reusable | `dsh-web-search-deepseek` | missing, owner decision pending (Q10) | backend, per-search credential, structured-block parsing | credentials bound to an origin and masked everywhere |
| Model-facing web tools (`web_search`, `web_fetch`) | none; planned `web_search`, `read_web_page` (#516) | — | `dsh-tool-web` | missing | both tools, untrusted-content labelling, HTML to text (rank 5) | web access can be withheld per role through grants (ADR-0050) |
| MCP client bridge (external tool servers) | none; a tool adapter behind the `Tool` contract (#695 names it as its own issue) | — | `dsh-mcp-client` | missing | whole client (rank 6); ACP `mcpServers` (#695) needs it; how an environment names run-time-discovered tools is open | — |
| MCP resource discovery and reading | none | — | `dsh-mcp-resources`, `dsh-mcp-client` | missing | depends on the MCP client | — |
| Workflow run service, events and ownership | `WorkflowService` (`p1-workflow/src/api.rs`: `StartRuns`, `ObserveRuns`, `CancelRuns`, `WorkflowObserver`); host implements `StepRunner`, `ModelResolver` | `p1-workflow`, `p1-host` | `dsh-workflow` | built | none | run journal with `resume_from`; per-model attempt caps and role fallback chains |
| Workflow script engine and sandbox | rhai engine in `p1-workflow` (ADR-0053, ADR-0114) | `p1-workflow` | `dsh-workflow-ptc`, `dsh-ptc-runtime-node`, `dsh-ptc-runtime` | different-by-design (ADR-0053) | none wanted (no JavaScript scripts) | scripts have no host access at all; caps are engine-enforced |
| Model-facing workflow tool | `Tool` port plus the `workflows` capability | `p1-tool-workflow` (modules `p1-module-workflow-start`, `-status`, `-result`, `-cancel`) | `dsh-tool-workflow`, `dsh-workflow-ptc` | built | no foreground mode (p1 always returns at once and notifies; not a recorded decision) | separate status, result and cancel tools; the module holds only `control` and `workflows` |
| PTC runtime (model-written programs that call tools) | none | — | `dsh-ptc-runtime`, `dsh-ptc-runtime-node` | optional, owner decision pending (Q6) | whole feature (served only by the web-app `ptc` preset) | — |
| Ralph loop (fresh agent per round toward one objective) | none | workflow scripts with verifier and judge roles | `dsh-tool-ralph`, `dsh-workflow-ptc`, `dsh-workflow` | optional | the packaged loop (dsh ships its base row `disabled: true`); expressible as a rhai script today | — |

## Family 8: file access, shell, sandbox and processes (10 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| File access service (resolve, read, list, atomic write, edit) | `runtime/capabilities.rs` (`WorkspaceService`, `SnapshotService`, `MutationService`) plus `modules/wit/workspace.wit`; not in `contracts/` | `runtime/file_services.rs` over `p1-workspace` (one backend) | `dsh-fs`, `dsh-fs-local` | partial, owner decision pending (Q5) | one backend; the traits move to `contracts/` when a second backend (remote) is decided; no watch | leaf replacement by exchange refuses a swapped-in entry; one `WriteGate` for all agents (ADR-0032) |
| Read-before-edit and stale-file guard | `ObservedFiles` (`p1-workspace`), `SnapshotService` | `edit`, `write` under the `WriteGate` (ADR-0025) | `dsh-fs-observation-policy` | built | no confirmed-absent state; whether observed state survives resume is unknown | freshness by content hash, not metadata; cannot be removed by configuration |
| File write fence and workspace confinement | `Workspace::resolve` (`p1-workspace`; ADR-0025, ADR-0122) | — | `dsh-fs-sandbox`, `dsh-sandbox-policy`, `dsh-fs-local` | different-by-design (ADR-0025) | a read-only file mode for review environments (no demand evidence); reads outside the workspace (#76) | reads are confined too; credential files unreadable; descriptor-based walk |
| Shell command execution (one-shot commands) | `Tool` port plus the `process` capability (`runtime/` `ProcessService`) | module `p1-module-shell` (`p1-shell-guest`), `p1-tool-shell` | `dsh-shell`, `dsh-bash-local`, `dsh-bash-sandbox` | built | no executor port for a non-bash runner (one implementation); no per-call cwd or stdin | closed request type (no env, cwd or sandbox per call); exit evidence is host-observed (ADR-0102) |
| Foreground timeout and background hand-over | `modules/wit/jobs.wit` (`process-jobs`) | `runtime/` `ProcessJobsService`, `p1-tool-shell-job` (ADR-0117, ADR-0123) | `dsh-shell` (`onExpiry`), `dsh-bash-local` | built | none | — |
| Managed shell environment (trusted facts for every command) | `ENV_ALLOW` allow-list in `runtime/` `ProcessService` (`with_env_pass`) | `p1-host` (`shell_entry`) | `dsh-shell-env` | partial | no session id or profile facts reach the shell (no consumer found) | allow-list instead of a name heuristic; injected snapshot, tests never read the real environment |
| Process sandbox: modes, per-call policy, fail-closed, escalation | none: a concrete `Sandbox` struct (`runtime/process/sandbox.rs`) and `--sandbox` flags | bubblewrap | `dsh-sandbox`, `dsh-sandbox-policy`, `dsh-bash-sandbox` | partial, owner decision pending (Q14) | read-only mode, per-session modes, escalation with approval, denial facts (rank 4); ACP modes #696 | home hidden behind a tmpfs; credential directories refused; a request cannot switch the sandbox off |
| Platform sandbox runners (bwrap, Landlock, Seatbelt) | none: a bubblewrap argument builder | `scripts/ci-bwrap.sh` (ADR-0097) | `dsh-sandbox-local`, `dsh-sandbox-windows-acl` | partial, owner decision pending (Q1) | a Landlock rung (rank 3); macOS and Windows undecided | launcher vetted against workspace-writable roots and re-validated per command |
| Subprocess service (managed process range, bounded output, escalated kill) | `runtime/capabilities.rs` (`ProcessService` trait) | native `runtime/process/mod.rs` (ADR-0091) | `dsh-subprocess`, `dsh-subprocess-local` | partial, owner decision pending (Q1) | cgroup or scope ownership for unsandboxed runs, terminal spawn (rank 11) | PID namespace when sandboxed; group-id reuse guarded |
| Persistent PTY terminal sessions | none (ADR-0117 excludes it) | — | `dsh-terminal`, `dsh-terminal-bash` | missing | PTY allocation, interactive input, signals, per-agent ownership (rank 24) | — |

## Family 9: approval, hooks, jobs and workspace (9 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Per-call authorization and ask (one-shot user approval) | `contracts/policy.rs` (`AuthorizationPolicy`, `Decision`); host `Asker` and `AskBridge` (`host/policy.rs`, host composition) | policy modules `p1-module-policy-full-access`, `p1-module-policy-ask`; `p1-host` | `dsh-user-approval` | built | no separate ask audit record in the journal (unknown); ACP `session/request_permission` #673 | the policy sees the call and its `Effect`; `Always` grants are keyed to verified digests and void on replacement |
| Permission presets (one selector over sandbox plus approval) | none: two flags fixed at start (`--ask`, `--sandbox`) | — | `dsh-permission-presets`, `dsh-user-approval`, `dsh-sandbox-policy` | partial, owner decision pending (Q14) | named presets and a per-session switch (rank 26); ACP `session/set_mode` #696 | confinement and credential refusal cannot be weakened by a preset |
| Model-based per-call authorization review | the `AuthorizationPolicy` port can host it | — | `dsh-experimental-auto-review` | optional | whole capability (dsh: experimental, web profile only, shipped switched off); would need a new capability grant | — |
| Hook protocol core (matcher, codec, merge, `hook/*` events) | none; the decision points are `AuthorizationPolicy` and `ContextPolicy` | `p1-hook-shadow` (detached, fail-open, ADR-0058) | `dsh-hook-protocol` | optional, owner decision pending (Q9) | block, inject context, force continuation from user scripts (dsh: a library used only by the two bridges, neither in a shipped bundle) | the one existing hook cannot affect a run or leak into the model |
| Claude Code hook compatibility bridge | none | — | `dsh-hooks-claude-code`, `dsh-hook-protocol` | optional, owner decision pending (Q9) | whole bridge (dsh: in no shipped bundle, added by hand); would be an optional policy or context module | — |
| Codex hook compatibility bridge | none | — | `dsh-hooks-codex`, `dsh-hook-protocol` | optional, owner decision pending (Q9) | whole bridge (dsh: in no bundle) | — |
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
| Incremental session-log upload to the official DeepSeek API | none | — | `dsh-session-log-deepseek` | missing, owner decision pending (Q3, Q10) | the vendor request extension | — |
| Durable host-side key-value storage | none: each persistent file has one owner | `p1-auth` store, `p1-workspace` `write_atomic`, journals | `dsh-storage`, `dsh-storage-domain`, `dsh-storage-json` | different-by-design (owner 2026-10-10: lead batch, each persistent file has one owner; no shared store or backend table) | none wanted until a second host-side store (a session index) exists; then decide | no hub, no process-wide backend table, no global store to migrate |

## Family 11: telemetry and feedback (8 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Session-event capture seam with redaction before hand-off | `contracts/policy.rs` (`EventSink`), `contracts/journal.rs` (`CommitSink`) | `p1-journal`, `p1-redact` | `dsh-session-telemetry` | partial, owner decision pending (Q3) | no outbound backend seam, no outbound redaction layer, no capture modes | credentials masked at the tool boundary, so the canonical log is clean |
| OpenTelemetry (OTLP) reporting channels | none | — | `dsh-otel`, `dsh-session-telemetry-otel`, `dsh-host-product-telemetry-otel` | missing, owner decision pending (Q3) | an exporter; would be an environment-named module so its absence means no code path | default-quiet: nothing leaves the machine |
| Feedback-authorized session-log upload | none | — | `dsh-session-telemetry-otel` | missing, owner decision pending (Q3) | consent-gated export | p1 uploads nothing |
| Product usage events (explicit analytics) | none | worker-observability rules (docs), `p1-usage` quota ledger | `dsh-host-product-telemetry-otel` | optional, owner decision pending (Q3) | product-event export (dsh mounts it only in `dsh-web-app`, disabled unless the profile is `desktop`) | analytics rules stricter in writing (no prompts, secrets or tool contents) |
| Anonymous installation identity | none | — | `dsh-anonymous-user-id` | missing, owner decision pending (Q3) | an install-scoped id | no install id on provider requests |
| Per-message ratings and notes | none | — | `dsh-message-feedback` | missing, owner decision pending (Q3) | ratings as journal record types | — |
| Session feedback command and dialog | none | — | `dsh-command-feedback` | missing, owner decision pending (Q3) | command and record | — |
| Session-log download (ZIP) | journals on disk; `scripts/run-report.py` | — | `dsh-session-log-export` | partial, owner decision pending (Q3) | an export command or archive, through an ACP client | no HTTP route to protect |

## Family 12: subagents, teams, schedule and experimental (9 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Start a child agent and get its result (one-shot delegation) | `WorkerService`, `WorkersStart` (`p1-workers/src/lib.rs`; optional module, ADR-0026); not in `contracts/` | `InProcessWorkers`; `p1-tool-delegate` (modules `p1-module-worker-start`, `-result`, `-continue`, `-cancel`, `p1-module-task`); `environments/subagents.toml` (ADR-0131) | `dsh-subagent`, `dsh-subagent-spawn-in-process`, `dsh-subagent-in-process-driver`, `dsh-tool-subagent` | partial, owner decision pending (Q8) | background or foreground choice, depth above one, other backends; the worker port moves to `contracts/` when a second backend is decided | an exact grant list; `worker_*` tools are never grantable (no recursion) |
| Per-call child model and effort choice | `ChildOptions`, `SubagentRequest` (ADR-0131) | `p1-workers`, `p1-host` | `dsh-tool-subagent`, `dsh-subagent-spawn-in-process`, `dsh-subagent-fork-in-process` | partial | a `list_subagent_models` tool; a route allow-list inherited by children | allowed models are a closed list per configured subagent |
| Fork: child seeded with the parent's completed turns | none | — | `dsh-subagent-fork-in-process` | missing | a child that continues the parent's conversation (ACP `session/fork` #691 is a session fork, a different thing) | — |
| Continue, steer, interrupt and list children | `worker_continue`, `worker_cancel`, `worker_result`; `WorkersObserve::list` exists on the trait | `p1-tool-delegate` | `dsh-tool-subagent-control`, `dsh-subagent` | partial | steer a running child, child-to-parent messages, a list tool, durable children (rank 22); #679 is main-turn steering | continuing a child cannot race a running turn |
| Agent teams (lead plus teammates, mailbox, task board) | none | workflows plus an ACP client | `dsh-experimental-agent-team`, `dsh-experimental-tool-agent-team`, `dsh-experimental-client-ui-agent-team`, `dsh-experimental-agent-team-profile` | optional, owner decision pending (Q8) | whole capability (dsh: experimental, shipped switched off, "an unstable prototype") | — |
| Scheduled reminders (once, interval, daily, weekly, cron) | none | an external scheduler starting a headless run | `dsh-experimental-schedule-bundle`, `dsh-schedule` | optional, owner decision pending (Q13) | whole capability (dsh: experimental schedule bundle, shipped switched off) | — |
| Inbound webhook that starts a new agent session | none | — | `dsh-webhook`, `dsh-webhook-github` | missing, owner decision pending (Q13) | whole capability | — |
| Speech-to-text service with a local recognizer | none | — | `dsh-experimental-speech-to-text`, `dsh-experimental-speech-to-text-sensevoice`, `dsh-experimental-api-speech-to-text` | optional, owner decision pending (Q4) | whole capability (dsh: experimental, shipped switched off) | — |
| Voice input in the front end | none | — | `dsh-experimental-voice-input-bundle`, `dsh-experimental-client-ui-voice-input` | optional, owner decision pending (Q4) | a client feature (dsh: experimental, shipped switched off) | — |

## Family 13: front ends, ACP, API and host (14 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| ACP agent door (stdio, standard protocol) | `FrontEnd` (`host/frontend.rs`, host composition) today; an ACP-neutral port in `contracts/frontend.rs` with a host session handle (#697, D7) | `p1-acp` (PR #687; own `wire::v1`, D6); the stdio driver #673 | `dsh-acp`, `dsh-acp-app` | in-flight #670 | the driver and `p1 acp` (#673); then #690 several sessions and close, #691 load, list, resume, fork, #692 plan updates, #693 titles, #694 images, #695 client MCP servers, #696 modes (D8: done only at dsh ACP parity) | a published extension surface under `_meta["p1.dev"]`; every `AgentEvent` mapped |
| SDK JSON-RPC stdio server (own wire protocol) | none by decision: no second automation protocol (#670 D1) | — | `dsh-sdk-protocol`, `dsh-sdk-jsonrpc-server`, `dsh-sdk-app`, `dsh-sdk-minimal` | different-by-design (#670 D1) | none wanted | one automation protocol to keep in step |
| Headless one-shot run (script and CI use) | `Command::Run` (`host/cli.rs`), `run_headless` (`host/run.rs`) | `p1-host` line front end (ADR-0043) | `dsh-headless` | built | no `--json` event stream; resume by id (#62); task from stdin | distinct exit codes: blocked, stalled, usage, cancelled; wait after transient failure (ADR-0041) |
| HTTP server for a browser UI | none | — | `dsh-host-webserver`, `dsh-host-frontend-static` | missing, owner decision pending (Q2) | none planned | — |
| Web GUI product (browser app, authenticated URL) | none | — | `dsh-web-app`, `dsh-web-frontend` | missing, owner decision pending (Q2) | whole GUI; if built, an ACP client | — |
| Typed browser-to-host RPC (gateway and remote catalogue) | none by decision: ACP plus `_p1/` extensions | — | `dsh-api-gateway`, `dsh-api-remotes` | different-by-design (#670 D1, design choices 5 and 9) | several sessions and reconnect (#690) | one protocol to secure and version |
| Session and workspace navigation API | none (start-time flags) | — | `dsh-api-session-controller`, `dsh-api-workspace-controller` | missing, owner decision pending (Q2) | session list, rename, archive (#62, #691); config options #675; workspaces as objects undecided | — |
| Human view of background jobs (list, follow, kill) | `runtime/jobs.rs` (`JobObserver`); `host/jobs.rs` | `p1-tool-read-output` | `dsh-api-job-controller` | partial | a human roster, follow and kill distinct from the model's; `_p1/agent_state` #683, `_p1/run/cancel` #682 | — |
| Human terminal pane (shell in the workspace) | none (a client concern under D1) | — | `dsh-api-terminal-controller` | missing | a client feature; no issue | — |
| Workspace file preview and watch (for the human) | `runtime/directory_listing.rs` (`DirectoryListingService`); the `read` tool | `p1-tool-read`, `p1-tool-ls` | `dsh-api-workspace-files` | missing | human preview and watch (client filesystem operations are not in the first ACP slice) | reads confined to the workspace plus one scratch root |
| Account, settings and credential screens | CLI: `p1 login`, `p1 logout`, `p1 usage` | `p1-host`, `p1-auth`, `p1-usage` | `dsh-api-account-controller`, `dsh-api-settings-controller` | partial | a redacted settings and credential surface for a client; screen-driven sign-in | no secret-returning path; no callback listener to secure |
| Workspace directory picker | none (the client sends `cwd` in `session/new`) | — | `dsh-host-directory-picker`, `-auto`, `-browse`, `-native` | missing, owner decision pending (Q2) | a client feature | — |
| Open workspace in an installed app | none | — | `dsh-host-open-in-app` | missing, owner decision pending (Q2) | a client feature | — |
| Plugin inventory (what is loaded, its state) | `p1 modules list`, `inspect`, `verify` (`host/modules_cli.rs`; ADR-0079, ADR-0087) | `p1-module-runtime` | `dsh-host-plugin-inventory` | partial | live state of a running host; a client-callable view | `verify` checks digests against the release manifest |

## Family 14: client UI (44 rows)

Checked by a smaller model only (survey `RUN.md`). Every row here is a client feature. p1's
own TUI and GUI are ACP clients (owner 2026-10-09; `p1-tui` and `host/tui.rs` are frozen), so
for most rows the port is the ACP wire plus the `_p1/` extensions and the adapter is a client,
not `p1-host`. "client" in the adapter column means exactly that.

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Browser-to-host transport (authenticated calls, live event stream) | ACP over stdio (#673); no HTTP listener | `p1-acp` | `dsh-client-connection` | in-flight #670, owner decision pending (Q2) | no socket; if one is ever added, dsh's Host and Origin fence rules apply | no network listener today |
| Raw file upload into an agent's attachment area | none | — | `dsh-client-file-upload` | missing, owner decision pending (Q11) | attachment intake, after image and file input | — |
| Live reload of a rebuilt plug-in bundle into an open page | none | — | `dsh-client-hmr` | different-by-design (owner 2026-10-10: lead batch, p1 ships no browser plug-in bundle to reload, ADR-0071) | none wanted | — |
| Plug-in bundle loading in the browser (boot graph, lazy table, live enable) | module names in `environment.toml` (ADR-0071) | `p1-host` | `dsh-client-modules` | different-by-design (ADR-0071, owner 2026-10-10) | none wanted | nothing registers implicitly |
| Installing, enabling, disabling browser plug-ins; inventory | verified releases plus the environment file (ADR-0065, ADR-0079, ADR-0087) | — | `dsh-client-ui-plugin-manager`, `dsh-client-ui-settings-plugins`, `dsh-client-ui-settings-plugin-inventory` | different-by-design (ADR-0065, ADR-0079, ADR-0087) | none wanted | module identity verified before loading |
| Observable state stores shared by UI packages | the journal as single truth (ADR-0021) | — | `dsh-client-store` | different-by-design (ADR-0021) | a client store may only project journal or ACP events | one source of state |
| Unified resource addresses resolving to live client values | none; `read_output` handles are the nearest | `p1-tool-read-output` | `dsh-client-resources` | missing | an address resolver for clients; no decision | — |
| Typed UI extension points (slots) | the `FrontEnd` trait at the root (host composition; becomes a port in #697) | `p1-host` | `dsh-client-ui-slots`, `dsh-client-ui-renderer`, `dsh-client-ui-session` | different-by-design (#670 D1 and D7, #697: one explicit trait at the root, becoming the front-end port) | none wanted | default no-op hooks let a line front end ignore what it does not render |
| Window frame, panels and viewing-state service | none (frozen TUI) | client | `dsh-client-ui-layout`, `dsh-client-ui-primitives` | missing, owner decision pending (Q2) | a new ACP client's job | the pure state machine and separate driver split (ADR-0043) is reusable |
| Light, dark and system theme with design tokens | none | client | `dsh-client-ui-theme` | missing, owner decision pending (Q2) | client | — |
| Interface language (host-stored preference, dictionaries) | none | client | `dsh-client-locale` | missing, owner decision pending (Q2) | client; no decision | — |
| Keyboard command registry and key routing | none | client | `dsh-client-shortcuts`, `dsh-client-ui-shortcuts` | missing, owner decision pending (Q2) | client | — |
| Slash commands and `@` triggers in the composer | frozen TUI commands; CLI flags; ACP `available_commands_update` #676 | client | `dsh-client-ui-commands`, `dsh-client-ui-input-trigger`, `dsh-client-ui-reference` | partial | a typed command and reference menu in a client; `@file` references (#687 has `embeddedContext` false) | — |
| Conversation shell and chat (layout, composer, node renderers) | `contracts/policy.rs` (`EventSink`); `FrontEnd::event_sink` | `LineFrontEnd`; ACP session updates (#670) | `dsh-client-ui-conversation`, `dsh-client-ui-chat` | in-flight #670 | ACP client renderers | — |
| Tool call cards (call tree, per-tool presentation slot, skill row) | `Tool::describe`, `describe_result` (ADR-0057, ADR-0059) | ACP `tool_call` updates (#687) | `dsh-client-ui-tool`, `dsh-client-ui-skill` | partial | client cards; the skill row after #129 | the host keeps no tool-name table |
| Cards for runtime-defined plug-ins | none by design (ADR-0103) | — | `dsh-client-ui-cordis` | different-by-design (ADR-0103, AGENTS.md) | none wanted | tools fixed at assembly by snapshot |
| Image and file attachments in composer, messages, trajectory | none | — | `dsh-client-ui-attachment` | missing, owner decision pending (Q11) | after image input (#694) | — |
| Timing ledger of one run with an interactive overview | `RequestTiming` records (ADR-0121) | `scripts/journal-timing.py` | `dsh-client-ui-trajectory` | partial | an interactive view in a client | the timing lives in the journal; a view is a pure reader |
| Changed-files card, per-file comparison, delivery cards | none (`ObservedFiles`; the `WorkspaceMutations` counter) | `p1-tool-edit`, `p1-tool-patch`, `p1-tool-write` | `dsh-client-ui-deliverables` | missing | per-file comparison and deliverables (rank 17) | — |
| Plan mode UI (controls, cards, previews) | none | — | `dsh-client-ui-plan` | missing | with plan mode (family 3) | — |
| Goal bar above the composer | none | — | `dsh-client-ui-goal` | missing | with goals (family 3) | — |
| Approval composer (frame-wide takeover) | host `Asker` (`host/policy.rs`, host composition; ADR-0024) | ACP `session/request_permission` (#673) | `dsh-client-ui-approval` | in-flight #673 | a client takeover | with no operator left, p1 denies and runs nothing unanswered |
| Questions to the user (composer takeover, plan review) | host `QuestionAsker` (`host/questions.rs`); `runtime/questions.rs` (`UserQuestionsService`) | `p1-tool-question`; ACP `elicitation/create` #674 | `dsh-client-ui-user-questions` | partial | a client composer; a plan-review intent | user-invocable only (ADR-0135) |
| Permission presets: default and per-session `/permission` | policy modules `p1-module-policy-ask`, `p1-module-policy-full-access` (ADR-0038) | — | `dsh-client-ui-permission-presets` | partial, owner decision pending (Q14) | a default and a per-session switch; ACP `session/set_mode` #696 | — |
| Message feedback (like, dislike, dialog, `/feedback`) | none | — | `dsh-client-ui-message-feedback` | missing, owner decision pending (Q3) | owner decision | — |
| Subagent conversations (catalog, continuation routing, `@` references) | `p1-tool-delegate` tools (ADR-0050, ADR-0131) | ACP worker transcript tagging #681, worker stop #682 | `dsh-client-ui-subagent` | in-flight #681 | browse child conversations and route continuations in a client | a worker gets only the tools its parent grants |
| Workflow run node with nested member disclosure | `FrontEnd` workflow hooks (`host/frontend.rs`; ADR-0075) | ACP `_p1/workflow_update` #680 | `dsh-client-ui-workflow-run` | in-flight #680 | the ACP tree update; whether a run tree survives restart is unknown | steps arrive as typed events |
| Model choice (catalog picker, Models page, onboarding) | `--model`, `--effort`; `p1-model-profile`; switching between turns (ADR-0049) | ACP config options #675 | `dsh-client-ui-model-selection`, `dsh-client-ui-settings-models` | partial | a catalog picker and onboarding in a client (#675) | — |
| Agent presets (default, this session's, composition editor) | environment files resolved by `p1-assembly` | — | `dsh-client-ui-agent-preset` | partial | an editor and a default-for-new-sessions in a client | validated into a resolved environment before any agent is built |
| Settings framework and General page | none (files and flags) | — | `dsh-client-ui-settings`, `dsh-client-ui-settings-general` | missing | a client settings UI; ACP config options #675 cover model and effort | — |
| Account: sign in to DeepSeek, sign out, billing pages | CLI `p1 login` (ADR-0040, ADR-0044) | — | `dsh-client-ui-settings-account` | partial, owner decision pending (Q10) | client sign-in and billing links | credential values masked everywhere; environment wins over the file |
| Settings pages for loop, shell, subagents and web search | environment keys (`[tool_concurrency]`; ADR-0131) | — | `dsh-client-ui-settings-agent-loop`, `-shell`, `-subagent`, `-web-search` | partial | client pages; web search absent (#516) | limits bounded in the environment file |
| Upload of session logs to the vendor API | none | `p1-journal` (local only) | `dsh-client-ui-settings-session-log` | missing, owner decision pending (Q3) | owner decision | no upload sink exists |
| Product event collection and reporting to the host | none (`p1-usage` is a quota ledger) | — | `dsh-client-product-analytics` | missing, owner decision pending (Q3) | owner decision | — |
| Session list (tree, search, grouping, state dots) | none: resume by path (ADR-0031) | `p1-journal` | `dsh-client-ui-sidebar` | partial, owner decision pending (Q2) | #62 list and ACP #691, then a client tree | a session file is owned before it is read |
| Right-hand sidebar dock | none | client | `dsh-client-ui-sidebar-right` | missing, owner decision pending (Q2) | client | — |
| Workspace file tree tab | `runtime/directory_listing.rs` (`DirectoryListingService`); `ls` (ADR-0115, ADR-0101) | `p1-tool-ls` | `dsh-client-ui-sidebar-files` | partial, owner decision pending (Q2) | a client tree | listings are bounded and refuse oversized output |
| Document previews (Office, spreadsheets, Markdown, images, PDF) | the `read` tool, text only | `p1-tool-read` | `dsh-client-ui-sidebar-documentpreview` | missing, owner decision pending (Q12) | owner decision | — |
| Sandboxed web browser tabs | none | — | `dsh-client-ui-sidebar-browser` | missing, owner decision pending (Q7) | owner decision | — |
| Interactive shell tabs | `shell`, `shell_job` (ADR-0117, ADR-0123) | `p1-tool-shell` | `dsh-client-ui-sidebar-terminal` | partial, owner decision pending (Q2) | needs PTY sessions (family 8) and a client | process-group kill and bounded output |
| Background job list in the session header | `runtime/jobs.rs` (`ProcessJobsService`, `JobObserver`) | `p1-tool-read-output` | `dsh-client-ui-jobs` | partial | a client list; `_p1/agent_state` #683 | job output redacted and paged by cursor |
| Open the workspace folder or a file in an installed application | none | — | `dsh-client-ui-open-in-app` | missing, owner decision pending (Q2) | client; owner decision | — |
| Choosing the workspace folder (browse, native chooser, picker) | `--workspace DIR`; confinement always on (ADR-0025) | `runtime/` listing service | `dsh-client-ui-workspace`, `dsh-client-ui-directory-picker-browse`, `dsh-client-ui-directory-picker-native` | partial, owner decision pending (Q2) | a client picker; ACP `session/new` carries cwd | no UI can move the file-tool root mid-run |
| Scheduled tasks and session reminders page | none | — | `dsh-client-ui-schedule` | optional, owner decision pending (Q13) | owner decision (dsh: experimental schedule bundle only) | — |

## Family 15: source-only packages: LSP, SSH, subagent backends (11 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Code navigation by language server (definition, references, implementation, hover) | none; if built: a tool module with the server process behind `ProcessService` and a host-side provider list at the root | text navigation only: `p1-tool-search`, `p1-tool-ls`, `read` | `dsh-lsp`, `dsh-lsp-stdio`, `dsh-tool-lsp` | missing | semantic navigation (rank 25); no issue | — |
| Remote workspace over SSH (files, processes, sandbox elsewhere) | the `runtime/capabilities.rs` traits (`WorkspaceService`, `ProcessService`, `SnapshotService`, `MutationService`) are the seam | native only | `dsh-ssh`, `dsh-fs-ssh`, `dsh-subprocess-ssh`, `dsh-sandbox-ssh` | missing, owner decision pending (Q5) | a remote backend is a host-side implementation of those traits, no tool change; the local-only choice is unrecorded | — |
| External-agent subagent backends (another harness as a worker) | `WorkerService` (`p1-workers`), one implementation | `InProcessWorkers` | `dsh-subagent-acp`, `dsh-subagent-dsh-sdk`, `dsh-subagent-claude-code`, `dsh-subagent-codex` | missing, owner decision pending (Q8) | a second `WorkerService` implementation or a `subagents.toml` entry kind; permission-mode mapping | exact grants; a leaf cannot get delegation tools; `finish` outcome host-observed (ADR-0102) |
| Programmatic client for a running harness (typed SDK) | ACP (`p1 acp`); the headless CLI | `p1-acp` | `dsh-sdk-client` | in-flight #670 | the ACP surface unmerged; no in-tree client library by decision (D1) | ACP has cancel and permission requests, which dsh's SDK client lacks |
| Alternative durable storage backend (SQLite) | `contracts/journal.rs` (`CommitSink`) | `p1-journal` | `dsh-storage-sqlite` | optional | no second medium; need unknown (no issue); dsh wires SQLite in no bundle | JSONL recovers a torn tail; writer lock before reading |
| Web search provider choice (Exa, Perplexity) | none (#516: host-owned backend) | — | `dsh-web-search-exa`, `dsh-web-search-perplexity` | missing | with #516; a normalized result type with an explicit omitted count | — |
| Session title generation (every prompt) | none | — | `dsh-session-title-all-prompts-llm` | optional | ACP titles #693 (after the first turn, again on change); dsh wires this package nowhere | — |
| Search and read earlier sessions from the model | none | — | `dsh-tool-session-query` | optional | a tool module over a host-held read service (the `tool-outputs` pattern); after #62 and #517 (dsh: opt-in model tools) | — |
| Persistent interactive terminal sessions | none (ADR-0117 excludes it) | `p1-tool-shell`, `p1-tool-shell-job` | `dsh-tool-terminal` | partial | a stateful terminal, send-input, signals, per-agent ownership (rank 24) | jobs end by notification; redacted output store; the sandbox masks credential directories |
| Build-time type-to-schema generation | WIT (`modules/wit/*.wit`) plus the JSON-schema bundle in `p1-module-protocol` | — | `dsh-typert-generator` | different-by-design (ADR-0071, ADR-0082) | whether the schema bundle is generated or checked by a test is unknown | wire values reject unknown fields |
| Developer hot reload of plug-ins | none by design (ADR-0071; nearest: ADR-0084, ADR-0078) | — | `cordis-plugin-hmr` | different-by-design (ADR-0071) | none wanted | — |

## Family 16: source-only packages: experimental, tests and apps (12 rows)

| Port | p1 contract (path) | p1 adapters (crates) | dsh counterpart | Status | Gap / next slice | Better than dsh |
|---|---|---|---|---|---|---|
| Browser use (model drives a real browser) | none | — | `dsh-browser-use`, `dsh-experimental-browser-use-runtime`, `-playwright-mcp`, `-chrome-devtools-mcp`, `-stagehand-native` | optional, owner decision pending (Q7) | would be a module plus a host-granted capability (dsh: experimental, shipped switched off) | — |
| Computer use (model drives the desktop) | none | — | `dsh-computer-use`, `dsh-experimental-computer-use-cua-driver-mcp`, `-native` | optional, owner decision pending (Q7) | whole capability; Linux only today (dsh: experimental, shipped switched off) | — |
| Programmatic tool calling in Python | none | rhai workflows differ (ADR-0053, ADR-0114) | `dsh-experimental-ptc-runtime-python` | optional, owner decision pending (Q6) | whole capability (dsh: experimental, mounted by no shipped profile) | the script engine is in-process rhai under caps, not an unsandboxed subprocess |
| Browser-hosted preview of the harness | none by decision (ADR-0071) | — | `dsh-experimental-webworker-runtime`, `dsh-experimental-webworker-packer` | different-by-design (ADR-0071) | none wanted | kernel namespaces for the shell, not a logical virtual filesystem |
| Live runtime inspection (DevTools view) | the journal (ADR-0021, ADR-0121) | — | `dsh-experimental-inspector` | optional | an ACP client feature; no issue (dsh: experimental opt-in bundle) | post-hoc inspection without a debug port |
| Web client boot and docking layout | none; the ACP door | `p1-acp` | `dsh-client-web`, `dsh-client-ui-dockkit` | different-by-design (#670 D1) | none wanted | one protocol for every screen |
| Desktop application (Electron shell and host process) | none; release channel (ADR-0065) | — | `dsh-desktop`, `dsh-desktop-host` | different-by-design (#670: a GUI is deferred behind ACP) | a GUI, when decided, as an ACP client | a verified release channel; the binary names its commit |
| Scripted model endpoint with fault injection | `ScriptedTransport` (`p1-provider-http::testing`) | `p1-provider-conformance` | `dsh-llm-mock-server` | partial | seeded fault mixing; a loopback endpoint for the real binary (ADR-0089 allows it) | deterministic faults, no sockets or ports |
| Recorded-session replay and keyless snapshot tests | `ScriptedProvider` (`p1-testkit`) | — | `dsh-llm-replay`, `dsh-session-snapshot` | partial | journal-to-script derivation and a snapshot refresh mode; ACP NDJSON fixtures planned (#670) | — |
| Agent-loop test harness | `p1-testkit` fakes for every contract | — | `dsh-agent-loop-testkit` | built | none | no clocks or I/O in the fakes; failure injection at an exact journal sequence |
| Whole-product boot smoke test | `p1-host/tests/cli.rs` and siblings spawn the binary | `p1-module-tests` | `dsh-loader-smoke` | partial | a reusable boot-and-run-one-turn helper | — |
| Client-side test doubles (Remote mock, client slot runtime) | none (no Remote, no slots); ACP fixture replay over a duplex pipe planned | — | `dsh-remote-mock`, `dsh-client-test-runtime` | different-by-design (#670 design choice 10) | none wanted | — |

## Feature gaps (not floor violations)

A feature gap is something dsh's default product does and p1 does not. It is not a floor
violation: the floor is modularity (owner 2026-10-10; "How to use this" above). Recounted by
script after the 2026-10-10 rework: 73 `missing`, 61 `partial` and 10 `in-flight` rows carry a
feature gap, 24 rows are `optional` (outside the floor, left out of the ranking), 31 are
`different-by-design` and 28 are `built`; of the `built` rows, 18 list remaining feature
differences in their Gap cell. The survey ranked the 30 gaps below by what a p1 user loses, in
this order of weight (accepted by the owner 2026-10-10): safety, correctness of their work,
ability to finish tasks, recovery from failure, cost. The ranking is the survey's judgement
(2026-10-10); the order of work is the lead's. Rank 20 (hook protocol core) left the list
after the rework: dsh ships no hook bridge in a bundle, so the row is `optional`; the other
rank numbers are kept as the survey gave them. Evidence per row is in the survey's "Top gaps
below the dsh floor" table (the survey's name for it; it predates the owner's reframing).

| Rank | Port (family) | Status | Gap in plain words | What a user loses | Owner |
|---|---|---|---|---|---|
| 1 | Overflow recovery (4) | missing | when the provider says the request is too long, p1 ends the turn; dsh compacts and retries once | recovery: a long task stops mid-way | no issue |
| 2 | Workspace instruction files (3) | in-flight #129 | instruction files load only by flag; no discovery, no global default, no budget, no journal record | correctness: project rules do not reach the model | #129 |
| 3 | Landlock fallback beside bubblewrap (2) | partial | one sandbox backend; where user namespaces are blocked the only option is no confinement | safety | no issue; Q1 |
| 4 | Process sandbox modes, per-call policy, escalation (8) | partial | no read-only mode, no per-session mode, no approved escalation, no denial facts | safety and finishing work | #696 (ACP modes); Q14 |
| 5 | Model-facing web tools (7) | missing | no web search and no web fetch | finishing tasks: no current documentation | #516 (deferred) |
| 6 | MCP client bridge (7) | missing | p1 cannot use external tool servers | finishing tasks | #695 names a split issue |
| 7 | Request retry policy and executor (5) | partial | a stream that fails after output has started cannot be re-run at the step | recovery | no issue |
| 8 | Image attachments (4) | missing | no image input at all | finishing tasks: no screenshots | #694 (ACP part); Q11 |
| 9 | ACP agent door (13) | in-flight #670 | `p1 acp` is not built yet | finishing tasks: no interactive door for editors | #673 |
| 10 | Task list tool (6) | missing | no task list the model and user both see | finishing tasks | #692 needs a source |
| 11 | Subprocess service (8) | partial | unsandboxed processes that leave the group survive | safety and cost | no issue |
| 12 | Tool output retention (6) | partial | only shell output is recoverable; other oversized results are cut | correctness | no issue |
| 13 | Skill catalog and loading (4) | partial | roots are flags, one level deep; no `skill` tool; no `/name` | finishing tasks | #129 |
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
| 26 | Permission presets (9) | partial | changing what the agent may do means restarting with other flags | safety | #696; Q14 |
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

## Owner questions the rows wait on

The survey raised these 14 questions on 2026-10-10 (full text: `SURVEY.md`, "Open questions
for the owner"). A row marked `owner decision pending (Qn)` waits on the question below; the
question is asked once there, not again by a session.

| Q | Topic |
|---|---|
| Q1 | operating systems beyond Linux (macOS Seatbelt, Windows, PowerShell) |
| Q2 | a browser or desktop GUI at all, and how it reaches the host |
| Q3 | telemetry, feedback and session upload off the machine |
| Q4 | voice input |
| Q5 | remote execution over SSH |
| Q6 | programmatic tool calling (model-written scripts that call tools) |
| Q7 | browser use and computer use by the model |
| Q8 | other harnesses as workers; agent teams |
| Q9 | hooks compatible with Claude Code or Codex |
| Q10 | DeepSeek as a first-class vendor (direct API, account, search, log upload) |
| Q11 | image and file input |
| Q12 | Office documents (skills, conversion, previews) |
| Q13 | scheduled and webhook-started runs |
| Q14 | model-requested sandbox escalation and a read-only mode |
