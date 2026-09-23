---
adr: 53
title: Workflows are an optional module: a sandboxed script orchestrates workers under roles and caps
status: accepted
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/adr/0050-every-main-agent-can-start-workers-a-worker-gets-exactly-the-tools-its-parent-grants.md, docs/adr/0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md, docs/adr/0049-model-selection.md, docs/design/delegation.md, docs/design/completion.md, scripts/workflow.py]
---
# ADR-0053: Workflows are an optional module: a sandboxed script orchestrates workers under roles and caps

## Context

Multi-agent work at scale (the 104-job modularity audit, review-then-verify sweeps) is
orchestrated today by `scripts/workflow.py` over `scripts/fanout.py`: a Python definition with
`agent()/parallel()/pipeline()`, schema-checked outputs, one repair round and resume. It runs
outside p1 and only the lead can use it. The owner, 2026-09-23: workflows "like they function in
Claude Code, but they must not be bound to a model or reasoning level. They should be a
possibility for all agents upon user request. … Like our subagent functionality must be a
module, this workflow feature must also be a module. … never use Fable en masse" (one Fable
reviewer per workflow, two or three for a big one). The owner's answers to the lead's questions:
a script an agent writes at run time (one new crate allowed for the engine); the tool always
mounted with a prompt rule "only when the user asks"; roles resolved by configuration with a
hard per-model cap the engine enforces; the implementation orchestrated by a Fable 5.1 Claude
Code session with DeepSeek/Opus workers. The owner then asked for a sanity check against what
peer harnesses ship: Claude Code's workflow tool has a deterministic script, schema retry, an
agent-count cap and a prefix cache — no required checks, workspace identity, budget groups or
fuel accounting; its verification is a script pattern. The design (lead + Astra,
`../phaseone-briefs/workflows-design-*.md`, v1 scope in `workflows-design-v1-scope.md`) was
trimmed to that shape plus the owner's two additions.

Engine choice on spike evidence (`../phaseone-spikes/{rhai,rune}-workflow/SPIKE-REPORT.md`,
DeepSeek workers, 2026-09-23): rhai 1.26 — sync VM, one OS thread per in-flight thunk, 931-line
bridge, 71 dependency lines, 196 s cold build, stable API, every escape vector blocked,
cancellation in microseconds; rune 0.14 — async VM but `!Send` values (one thread per run
anyway), ~900-line bridge with three fragile spots (cancellation by matching an error string,
a `from_value::<String>` ownership trap, raw stack access for variadic functions), 131
dependency lines, 365 s cold build, pre-1.0. rhai is the stable, smaller choice; its cost is
bounded by the worker pool and a thread cap.

## Decision

1. **Three modules, nothing in the core.** `p1-workflow` owns the script engine (rhai, the one
   new crate, pinned), the script API, roles and caps resolution, envelopes, the journal and
   replay, cancellation and the `WorkflowService` trait; it depends on no tool, not on
   `p1-workers` and not on `p1-core` — it drives steps through a `StepRunner` trait it owns.
   `p1-tool-workflow` is the tool module: `workflow_start {script, args?, resume_from?}`,
   `workflow_status`, `workflow_result`, `workflow_cancel`. The host composes them with ordinary
   constructors and implements `StepRunner` over `p1-workers`.
2. **A step is a worker.** Each `agent()` call starts one worker through a prepared-start seam
   in `p1-workers` (capacity reserved and id allocated before the child is built); capacity is
   shared with direct workers. A step gets exactly its role's or the call's tool grant (opt-in,
   at least one, never the worker or workflow tools) plus `finish`. One level (ADR-0050).
3. **Script API = Claude Code's shape.** `agent(prompt, opts) → envelope`, `parallel([thunks])`,
   `pipeline(items, stage…)`, `phase`, `log`, `args`. The envelope is `{step, status, value,
   schema, evidence, attempts, worker, error}`; the host wraps the script's return in a run
   envelope with counts (failed, blocked, not verified, capped) so a script cannot hide failed
   workers. The VM has no time, randomness, file, network, environment, `eval` or import: the
   engine is built raw with only the reviewed packages; `sleep` is overridden; operation, call
   depth, expression depth, string, array, map and function limits are set; the thunk thread
   pool is bounded.
4. **Roles and caps, not models.** A script names roles. `settings.toml` maps each role to
   `environment/profile[:effort]` and a tool grant, and sets `[workflows.caps]` per resolved
   `wire_model`, counted in attempts (starts and repairs) per run. The (cap+1)th attempt fails
   with a typed `quota_exceeded`; no substitute model. On `resume_from` the counter is rebuilt
   from the journal. Shipped defaults: worker and reviewer on Claude Opus 5.5, verifier on
   deepseek2, judge on Claude Fable 5.1 with cap 3.
5. **Structured output through `finish`.** A `result` field on `finish`, validated in Rust
   against the host-supplied schema subset after an accepted `done`; one repair turn in the same
   child; then `failed: invalid_output`. Evidence per step is ADR-0051's label.
6. **Journal and replay.** Every call is journalled (stable call id from call site + prompt +
   opts, never a sequence number, because `parallel` reaches `agent()` in a different order each
   run) before dispatch, append-only. `resume_from` replays the longest unchanged prefix.
7. **Host.** The workflow tools are appended to every main agent (as ADR-0050 does for the
   worker tools) with a prompt section that offers, never pushes: "only when the user asks for a
   workflow", with a short example. One host line per step; ONE inbox notification to the parent
   at the end; a run directory with `journal.jsonl` and `result.json`; `p1 workflow run FILE
   [--role r=E/P:effort] [--arg k=v]`; background while the host lives.
8. **Deferred, explicitly:** required checks run by `finish`, workspace leases and artifact
   identity, budget groups, a durable quota ledger, fuel accounting beyond the engine's limits,
   detached execution, sub-workflows. Each returns only when a real run shows the need.

## Consequences

- Every main agent, on any model, can run a workflow when the user asks; no model or effort is
  named in a script. Fable cannot be used en masse by any script: the cap is enforced.
- `scripts/workflow.py` stays the external runner until the reduced modularity audit runs
  through `p1 workflow run` with the same results; then it is retired.
- Cost: one crate (rhai) and ~3 minutes of cold build; one OS thread per in-flight thunk,
  bounded.
- Verification is a script pattern (checker steps, refuter votes), not an engine feature; the
  envelope makes unverified and failed steps visible whatever the script returns.

## Alternatives considered

- A declarative graph (JSON/TOML): no new crate, but conditionals and loops become awkward; the
  owner chose a script.
- Keep `workflow.py` as the engine behind a tool: fastest, but p1 would need Python at run time.
- rune or a JS engine (boa): see Context; rune's `!Send` values negate its async advantage,
  boa is far heavier.
- Required checks and workspace identity in v1: what no peer ships and what the owner's
  sanity check removed.

## Evidence

Implemented 2026-09-23 by a Fable 5.1 Claude Code orchestrator session with one worker per job
(brief `../phaseone-briefs/workflows-orchestrator.md`, report
`workflows-orchestrator-report.md`): API f099e51, job 1 1c56a49, job 2 cb51b60, job 3 d7c0066,
job 4 af68a55, job 5 2e1b471, docs a291c43, audit port d430f7e (race fix 31dc790); every
landing gate + CI green, main green on 45411f1. Runs in `docs/dogfood/runs.jsonl` (wf1–wf6,
audit, repairs) and in the model-cards evidence file. Live checks, all passed: (1) the same
small workflow on a deepseek2 and on an Opus 5.5 main agent, one inbox notification each;
(2) the cap refusing the (cap+1)th attempt before dispatch for gpt-6-sol at 3 and for Fable
at 1; (3) schema repair in the same worker, in-turn and via the engine's repair turn;
(4) `resume_from` after editing one call — step 1 replayed, 2 and 3 re-run; (5) the reduced
modularity audit through `p1 workflow run` — 4/4 steps with `commands passed`, a split vote
resolved by a third vote, one real finding (issue #46). Found and fixed on the way: a
`continue_child` ordering race in `p1-workers` (a9a6967) and a dying child task stranding
every `wait` (4822e30); found and filed: #53 (stall guard blind to shell-made writes), #54
(workers that change nothing must still call `finish`).
