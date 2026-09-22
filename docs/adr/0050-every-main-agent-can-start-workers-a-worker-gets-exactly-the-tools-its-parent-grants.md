---
adr: 50
title: Every main agent can start workers; a worker gets exactly the tools its parent grants
status: accepted
date: 2026-09-22
deciders: owner+lead
supersedes: [26]
superseded_by: []
sources: [docs/adr/0026-delegation-is-optional.md, docs/design/delegation.md, docs/design/pillars.md, crates/p1-tool-delegate/src/lib.rs, crates/p1-host/src/run.rs]
---
# ADR-0050: Every main agent can start workers; a worker gets exactly the tools its parent grants

## Context

ADR-0026 made delegation an optional module "assembled only when configured", which in practice
became a second environment per family: `claude-delegating` is `claude` plus the four worker
tools and one prompt section. Model selection (ADR-0049) lists every environment × profile, so
every Claude model appears twice in `p1 models`. The owner, 2026-09-22, corrected the reading of
the 2026-09-19 direction ("orchestration: supported and optimised for, never imposed"): it
rejected an orchestrator MODE that pushes the agent to delegate everything; it never meant the
capability should be hidden. The owner also wants the parent to decide what a worker can do —
by actually assembling only the granted tools, not by telling the worker what not to use and not
by refusing calls — and a worker that lacks a tool must not fail silently (it has happened: a
worker failed and the main agent did not report it).

## Decision

1. **Every main agent has the worker tools.** The host adds `worker_start`, `worker_result`,
   `worker_continue` and `worker_cancel` to every top-level agent it assembles (when the
   `delegation` feature is compiled). Environment files do not list them. The
   `claude-delegating` environment is removed; each model is listed once.
2. **The prompt offers, never pushes.** Every shipped main-agent prompt carries today's
   "Workers (optional)" section, shown only when the worker tools are assembled (conditional
   prompt sections, item 4). No other text steers toward delegation.
3. **Opt-in tools.** `worker_start` takes a REQUIRED `tools` list (at least one entry) of tool
   module names. The worker is assembled with exactly those tools plus `finish`, which every
   worker gets because it is how a worker reports done or blocked. Any tool module p1 ships may
   be granted except the worker tools; the environment's own `[[tools]]` list does not limit or
   extend the grant (an entry there only supplies the face the tool is presented under). A tool
   the worker's route cannot express (e.g. freeform `apply_patch` on a function-only route) makes
   the start fail with the reason; nothing is started. The schema lists the valid module names
   and environment names as enums.
4. **Conditional prompt sections.** Assembly gains `{{#tool:<module>}} … {{/tool:<module>}}`: the
   enclosed text is kept when the module is assembled and dropped otherwise. Shipped prompts wrap
   every tool-specific instruction this way, so a worker's prompt mentions exactly its tools.
5. **One level.** A worker never gets the worker tools. A main agent that needs a delegating
   sub-agent (e.g. one model orchestrating a workflow) starts it as its own interactive p1 session
   in a tmux pane; that is separate, later work.
6. **A short-handed worker is visible without the parent's cooperation.**
   - The worker prompt tells it to finish `blocked` with `needs` naming a missing tool.
   - The worker service records, per worker, its granted tools, its `finish` status and `needs`,
     and every call to a tool it was not given; `worker_result` returns them as structured lines
     ahead of the final text.
   - The host renders every worker's end itself (line output, TUI, run report), e.g.
     `worker w1 (claude; read, grep) blocked: needs edit — tried edit x2`, whatever the parent
     later says.
   - `worker_continue` accepts `add_tools`: granted between turns through `Agent::reconfigure`
     (ADR-0049), so a repair keeps the worker's context.
   - The `worker_start` description says: list every tool the task needs; if unsure, include it.

## Consequences

- One row per model in `p1 models`; delegation is no longer an environment property.
- Workers become narrower by default (explicit grants), and their prompts shrink with them.
- Prompts must be written with conditional sections; the prompt-coherence test checks that every
  mention of a tool module sits inside that module's section or the tool is always assembled.
- Headless runs of every environment can now delegate; the cost of a worker is still bounded by
  `max_concurrent` and the stall guard.

## Alternatives considered

- Keep a delegating environment per family, or hide it from the list: keeps the duplication as a
  concept and still decides delegation by environment, not by the parent.
- Opt-out grants (a worker gets everything unless told otherwise): the owner requires a
  deliberate choice.
- Refusing calls to ungranted tools, or prompt instructions: the owner requires the tool to be
  absent.
- Named worker profiles (preconfigured grants): wanted later, on top of the list.

## Evidence

Merged 2026-09-22 (CI green on `97ea43f`): `task/prompt-sections`, `task/worker-grants`,
`task/main-agent-workers`, `task/worker-report`, `task/worker-add-tools`, all implemented by DeepSeek
V4.1 Flash workers through p1 and reviewed by the lead. Offline: `cargo test -p p1-assembly --test
lead_prompt_coherence --test prompt_sections`, `cargo test -p p1-host --test worker_grants --test
worker_report --test worker_add_tools`, `cargo test -p p1-tool-delegate -p p1-workers`.

Live, lead, `claude/claude-opus-5-5` main agent: it started a `deepseek2` worker granted only
`read` (assembled: read, finish) to change a file; the worker finished `blocked` naming a writing
tool and the host printed `· worker w1 (…; read, finish) blocked: needs …` on its own; the parent
granted `edit` with `worker_continue add_tools`, the same worker (context kept) made the change,
and the parent verified the file. `p1 models` lists each Claude model once.

Found live: a worker granted `edit` but not `shell` cannot finish `done` — the ADR-0037 finish
check requires a recorded successful command, which it cannot run — so it finished `blocked`
asking for a shell after a correct change. Follow-up decision recorded outside this ADR.
