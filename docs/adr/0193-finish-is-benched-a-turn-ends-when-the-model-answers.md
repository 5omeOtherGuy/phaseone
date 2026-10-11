---
adr: 193
title: finish is benched; a turn ends when the model answers
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: [808, 766, docs/adr/0037-unattended-runs-end-by-an-observable-finish-call-with-bounded-continuation.md, docs/adr/0042-a-headless-run-that-only-summarizes-ends-as-stalled.md, docs/adr/0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md, docs/adr/0072-a-workflow-step-that-ends-without-finish-gets-one-repair-turn.md, docs/adr/0120-an-accepted-finish-ends-the-turn-without-another-request.md, docs/adr/0163-repeated-finish-verification-refusals-end-blocked.md, docs/design/completion.md]
---
# ADR-0193: finish is benched; a turn ends when the model answers

## Context

ADR-0037 made completion an observable act: every shipped environment listed the `finish`
tool, a headless run continued a turn that ended without an accepted `finish` and could end
`stalled`, every worker was granted `finish` on top of its grant, and a workflow step that
ended without it got a repair turn (ADR-0072). ADR-0042, ADR-0051, ADR-0120 and ADR-0163
build on that. The owner decided on 2026-10-11 (#808): "keep finish as a wasm tool we can
load but we put it on the bench and might use it later. We don't use it at all now."

## Decision

1. **No shipped configuration uses `finish`.** No shipped environment lists it and no
   shipped prompt asks for it. The `p1/finish` module and its native twin `p1-tool-finish`
   stay built, shipped, loadable and tested, unchanged: an environment that lists
   `module = "finish"` gets today's behaviour back (the bench).
2. **A turn ends when the model answers** without tool calls, in every front end, headless
   included. Without an assembled `finish` the headless host sends no continuation message
   and never ends a run `stalled` for stopping without finishing; it still waits for a
   pending inbox and for running workers, and the §3c summary guard still applies.
3. **A worker gets `finish` only when its own environment lists it.** The grant is exactly
   the modules the parent named; the `worker_start` description no longer says "plus
   finish". Its started message still names `finish` after the grant, because the frozen
   ACP fixture `docs/acp/fixtures/workflow-run.jsonl` pins that text; the module cannot see
   the child's environment, and changing the text is the owner's call.
4. **A worker without `finish` is reported by how its turn ended**: `completed`, `failed` or
   `cancelled` in the host's worker line; `worker_result` shows no `finish:` line. A workflow
   step whose worker has no `finish` and whose turn completed is `done`, its last message the
   value and its evidence `not verified; parent verification required`; it gets no repair
   turn. A step that passes a `schema` needs `finish` to return the structured result, so it
   fails before any worker runs when the role's environment does not list `finish`.
5. **Narrowed, not reversed.** ADR-0037 and ADR-0042's `finish` progress, ADR-0051's
   completion policies, ADR-0072's repair turn, ADR-0120's turn ending and ADR-0163's bound
   apply only to an agent whose environment lists `finish`. Their texts stand for that case.

## Consequences

- An unattended run of a shipped environment ends at the model's first answer. A model that
  stops early ("shall I continue?") ends the run with exit 0; the guard against that (F1)
  now lives only in the prompts.
- A worker's or step's `done` is never verified by the host: the parent verifies, as it
  already had to for a `ReportToParent` result.
- The shipped prompts lose their `# Finishing` sections with `finish`; the prompt fragment
  `environments/prompts/fragments/finish.md` stays, listed with `requires = ["finish"]`, so it
  renders only for an environment that lists `finish`.
- Workflow scripts that use `schema` need a role on an environment that lists `finish`.

## Alternatives considered

- Deleting the module and its host plumbing: rejected by the owner ("might use it later").
- Keeping `finish` granted to every worker: rejected, it would still be used by every
  delegated run.
- Granting `finish` only to schema steps: rejected, it would keep `finish` in shipped use
  through workflow scripts; a schema step on a benched environment fails loudly instead.
- Parsing a step's final text as JSON against its schema: a new contract, not decided here.

## Evidence

`cargo test -p p1-host --test review_environment`
`the_review_environment_reports_to_scratch_and_ends_on_its_answer` (the shipped
`deepseek-review` environment, headless: two requests, exit 0, no continuation, no
`finish` in its tools) and `--test completion` `h_an_environment_without_finish_is_unchanged`;
`--test worker_report` `a_worker_without_finish_is_reported_by_its_turn_end`;
`--test workflow_schema` `a_step_without_finish_is_done_by_its_turn_end_without_a_repair`
and `a_schema_step_needs_an_environment_that_lists_finish`. The existing finish suites
(`completion.rs`, `finish_ends_turn.rs`, `worker_report.rs`, `worker_evidence.rs`,
`workflow_*.rs`, `p1-module-tests/tests/finish_boundary.rs`) run unchanged against scratch
environments that list `finish`. `cargo test -p p1-assembly --test lead_prompt_coherence`
checks that no shipped environment assembles `finish`.
