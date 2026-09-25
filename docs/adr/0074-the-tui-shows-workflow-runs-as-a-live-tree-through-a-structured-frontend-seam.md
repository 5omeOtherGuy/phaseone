---
adr: 74
title: The TUI shows workflow runs as a live tree through a structured FrontEnd seam
status: proposed
date: 2026-09-25
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [issue #198]
---
# ADR-0074: The TUI shows workflow runs as a live tree through a structured FrontEnd seam

## Context

The owner ordered, 2026-09-25 ~14:20: "I want the live progress tree in the side
pane." The WORKERS pane (#111) is a flat list of workers. A workflow run reaches the
TUI only as text lines through `FrontEnd::workflow_line`, so the pane cannot show what
issue #198 wants: the run, its phases, the steps in each phase, and which worker is
doing which step, live. The TUI must learn this without a dependency on `p1-workflow`
(§7.7): it gets a data contract it owns, filled by the host across the existing
`FrontEnd` seam.

## Decision

1. **Data contract owned by p1-tui.** `crates/p1-tui/src/workflow.rs` holds plain
   structs with strings, no `p1-workflow` types — the §7.7 rule, the same pattern as
   `p1_tui::transcript::WorkerReport`: `RunStarted { id, resumed_from }`,
   `StepStarted { run, call, label, phase, role, model /* "E/P:effort" */, worker_id:
   Option<String> /* "w3" */, attempt, prompt }`, `StepEnded { run, call, label, model,
   status /* done|failed|blocked|cancelled */, attempts, replayed, error, worker_id }`,
   `RunEnded { id, outcome, steps_started, steps_ended, steps_failed, error }`, and a
   `WorkflowEvent` enum that also carries `Phase(run, name)`, `Log(run, text)`,
   `JobsQueued(run, count)`, `ThunkFailed(run, error)`. `label`/`model` were added to
   `StepEnded` because a replayed or refused step never has a step start, and its row
   still needs a name.
2. **FrontEnd seam.** `crates/p1-host/src/frontend.rs`, feature `workflows`, gets new
   methods with default no-ops: `workflow_run_started`, `workflow_phase`,
   `workflow_log`, `workflow_jobs_queued`, `workflow_step_started`,
   `workflow_step_ended`, `workflow_thunk_failed`, `workflow_run_ended`.
   `workflow_line` is unchanged: the line/ledger text keeps its exact wording. The
   headless/line front end ignores the new calls (defaults); the TUI front end forwards
   each as a stamped `UiEvent::Workflow` through the same channel the worker events use
   (`TuiSink`), so the tree is stamped on the TUI's one clock (the sink's millisecond
   epoch, which a paused test runtime drives) — no wall-clock read in the renderer.
3. **Projection and the early step announcement.** `HostWorkflowObserver` projects
   every observer event into those calls (`run_started`, `phase`, `log`,
   `step_started`, `step_ended`, `thunk_failed`, `run_ended`, and the new
   `jobs_queued`). Because the engine calls `WorkflowObserver::step_started` only after
   the step's first turn returned (it needs the `WorkerRef`), the host's step runner
   (`HostStepRunner`) also announces `workflow_step_started` the moment the step's
   worker exists (right after `start_prepared`), so the tree shows a live step with its
   worker; the observer's later call is an idempotent update of the same row (keyed by
   run + call while the row is running; a fallback link's new worker replaces the
   worker id).
4. **`jobs_queued` on the engine.** A new `WorkflowObserver::jobs_queued(&self, id:
   &RunId, count: usize)` (default no-op) is called once when `parallel`/`pipeline`
   starts its fan-out, with the number of jobs. The TUI's queued count = jobs queued −
   steps that appeared since, floored at 0; totals grow, never shrink.
5. **Tree and rendering rules** (`crates/p1-tui/src/render/workers.rs`): when at least
   one run exists the pane is the tree: runs in start order → phases in order (current
   phase marked) → steps in order → the step's worker block (the #111 block, indented)
   under a RUNNING step; after the step ends the worker folds into the step row.
   Workers no step references stay in a flat `workers` group after the runs,
   unchanged. Run header: `wf1 · <phase or —>` (+ ` ↺ wf0` when resumed) with elapsed,
   then counts `running · done · failed · queued · total`, then summed tokens and tool
   calls; dim under it the last log line and a thunk-failure note. Phase rows
   `▾ Review 2/5` (open, done/known) / `▸ Review 5/5` (collapsed). Step rows: glyph
   (▪ running, ✓ done, ✗ failed, ⊘ blocked/cancelled, ↺ replayed), label or call id,
   model `E/P:effort`, `×2` when attempts > 1, elapsed live then final; a second line
   at width ≥ 48 with the current activity (`<tool> <first argument>`, `streaming`,
   `thinking`; after the end the outcome word or the error's first line) and
   tokens/ctx and tool calls (cost too at ≥ 56); below 48 the activity is a dim
   suffix. Tokens and tool calls are summed on phase and run; a sum with an unknown
   part shows `+?`, all-unknown `—`, never 0 for unknown. Tool calls are the TUI's own
   count of `ToolStarted` events on the worker's event stream. Collapse: when the pane
   height cannot show every row, ended phases (no running step, not the run's current
   phase of a running run) collapse to one row, oldest first, until it fits; the
   running phase and the selected row's phase never collapse. Compact (< 56) and
   normal layouts as #111.
6. **Navigation and retention.** Selectable rows are run headers, steps and worker
   blocks (phase rows are not); ↑/↓ as #111; ⏎ on a step opens it (a stats band:
   status · model · phase · attempts · elapsed · tokens · tool calls, the prompt
   folded to 3 lines with `p` toggling the full wrapped prompt, then the live
   transcript); `a` attaches the step's worker transcript directly; a step with no
   worker yet does nothing; esc detaches; `x` on a step asks to stop its worker (the
   existing worker stopper), `x` on a run header asks `cancel wf1?` and `y` calls the
   workflow service's `cancel(id)` through a host hook next to the worker stopper.
   Focus rules of #92/#150 unchanged. Retention: the TUI keeps every worker's
   transcript in memory for the session (`Screen::worker_transcripts`, never pruned)
   and the tree for the session, so an ended step stays openable; nothing is
   persisted.

## Consequences

The TUI still learns no `p1-workflow` type; ledger lines are unchanged. A pipeline's
job count is per item, not per stage, so the queued count is an estimate that only
reaches 0 as steps appear. Redraw: a running run keeps the heartbeat drawing once a
second so live elapsed moves. The engine, the host's `WorkflowObserver` trait and the
`FrontEnd` trait each gain one more method to implement (with defaults), and the
host's step runner now announces a step twice (once early, once from the observer) —
the projection must stay idempotent on the same row.

## Alternatives considered

- Parse `workflow_line` strings in the TUI: rejected — §7.7 forbids the TUI decoding
  domain strings, and it is fragile against wording changes.
- Give `p1-tui` a `p1-workflow` dependency so it can hold engine types directly:
  rejected — the crate boundary of §7.7 exists precisely so the UI never depends on
  the engine.
- Poll `WorkflowService::status` from the TUI like the worker refresher polls workers:
  rejected — `RunProgress` carries no per-step data, and polling loses the ordering of
  phase and step transitions that the tree depends on.

## Evidence

- `crates/p1-tui/tests/workflow_tree.rs`: the run header's five counts, a phase row's
  `2/5`, a running step's tool call and worker block, a failed step on its second
  attempt and a replayed step, the flat `workers` group, collapse under a short pane
  (never the selection's phase), widths 30/46/48/60, selection across headers, steps
  and workers, `a`/`⏎`/`p` on a step, `x` on a step and on a run header.
- `crates/p1-tui/src/workflow.rs` unit test
  `a_repeated_start_updates_the_running_row_and_a_replay_makes_its_own`: the upsert of
  a repeated step start and the queued arithmetic.
- `crates/p1-host/src/workflow.rs`
  `every_observer_event_is_projected_into_the_structured_calls`: every observer event
  (including `jobs_queued` and `thunk_failed`) becomes its `FrontEnd` call, and the
  ledger lines keep their wording.
- `crates/p1-host/src/tui/tests.rs`
  `workflow_calls_build_the_tree_and_a_run_header_cancels_through_the_run_channel`: the
  TUI front end forwards the calls to the screen's tree and `x`/`y` on a run header
  reaches the host's run canceller.
- `crates/p1-workflow/tests/runs.rs` `a_fan_out_reports_its_job_count_when_it_starts`:
  `parallel` and `pipeline` report their job counts.
