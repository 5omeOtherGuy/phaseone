---
adr: 51
title: A worker without a command tool may finish done; the result says it was not verified
status: accepted
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/adr/0037-unattended-runs-end-by-an-observable-finish-call-with-bounded-continuation.md, docs/adr/0050-every-main-agent-can-start-workers-a-worker-gets-exactly-the-tools-its-parent-grants.md, docs/design/completion.md, crates/p1-tool-finish/src/lib.rs, crates/p1-host/src/catalog.rs, crates/p1-host/src/activity.rs]
---
# ADR-0051: A worker without a command tool may finish done; the result says it was not verified

## Context

ADR-0037 defines `done` as "verified by a command after the last file change", checked by the
`finish` tool against the session's recorded shell runs. ADR-0050 lets a parent grant a worker
any subset of tools. The two collide: in the ADR-0050 acceptance run a DeepSeek worker granted
`[read, edit]` made the correct change and then could not finish `done` — it had no tool that
runs a command — so it finished `blocked` asking for `shell`. Correct work was reported as a
blocker, and a parent that trusts the report would either abandon the result or grant `shell`
to every worker, which defeats opt-in grants.

Two events were conflated: a worker completing its assignment, and someone establishing that
the result is right. The command gate is one way of establishing it; a worker that cannot run
commands has no way, and must still be able to end honestly. The owner, 2026-09-23, chose this
small change first ("add the small fix now") and, with the lead and Astra's review
(`../phaseone-briefs/finish-design-answer.md`), rejected granting `shell` implicitly.

## Decision

1. **`finish` has a host-chosen completion policy.** `FinishTool` takes a policy at
   construction; the default is today's rule (ADR-0037, unchanged for main agents and for any
   worker with a command tool). The second policy, `ReportToParent`, accepts
   `{status: done, verification: ["none"]}` even after file changes. The host selects it for
   a worker whose assembled tools include no tool that records command runs, and selects it
   again on every re-grant (`worker_continue add_tools shell` puts the worker back on the
   strict rule for its next turn). The host decides from the assembled tools' identities, the
   way `WorkerReportTap` finds `finish`; the tool never inspects grant names.
2. **Evidence is host-owned.** The accepted outcome carries what was established:
   `Accepted::Done { summary, evidence }` with `evidence` one of `CommandsPassed(commands)` or
   `NotRun(reason)`. Under the strict rule `["none"]` on a session that changed no files is
   accepted as before but recorded as `NotRun("no file changed")`: not writing a file is no proof
   that an answer is right. A named command that fails the existing checks is still rejected
   under both policies — invalid evidence never downgrades to an accepted unverified result.
3. **The label is shown without the parent's cooperation.** `WorkerReport` gains the evidence
   (taken from the accepted outcome, not from the model's input). `worker_result`, the host's
   own worker line, the TUI note and the run report show `done — not verified; parent
   verification required` (or `done — commands passed: …`). "Verified" is never printed for a
   result without passed commands.
4. **Everything else stays.** `blocked` keeps its meaning (a read-only worker asked to edit is
   blocked); a worker with no tools is still rejected (ADR-0050); the tool description and the
   worker prompt explain the policy that applies; the host's headless exit codes are unchanged.
5. **Not in this change:** parent-defined checks run by `finish`, completion policies chosen
   per task, worker profiles. They are the next design (workflows and profiles) and get their
   own ADR; this decision must not pre-empt it, so the policy is a value the host selects, not
   a rule derived inside the tool.

## Consequences

- A restricted worker ends with a true report. The parent, a checker worker or a script decides
  whether to accept the work; a child's `done` alone never satisfies the parent's own gate.
- Two labels a reader must not confuse: `commands passed` (a command the worker chose succeeded
  after its last change — not proof of correctness) and `not verified` (nothing ran).
- Amends ADR-0037's consequence ""Done" now means verified by a command": it now means that
  only under the strict policy, which stays the default for every main agent.
- The frozen completion tests of ADR-0037 stay as they are; new tests cover the second policy.

## Alternatives considered

- Grant `shell` with every `finish`: defeats opt-in grants; a read-only worker could write.
- Accept a read-back of the changed file as verification: theatre, proves nothing.
- Let `finish` run a parent-defined check: the right next step, but a bigger patch (a verifier
  interface, workspace, limits, cancellation) — deferred to the workflow design.
- A separate checker worker after every implementer: already possible under ADR-0050; useful
  as a pattern, wrong as an imposed rule, and it does not let the implementer end.

## Evidence

`scripts/gate.sh` green on `5121536` (merge of `task/finish-policy`): `cargo test -p
p1-tool-finish` (new `tests/policy.rs`, the frozen `tests/finish.rs` changed only by the new
`evidence` field in its existing assertions) and `cargo test -p p1-host --test
worker_evidence` (six scenarios: no command tool → done/not verified; fabricated command still
rejected; a worker with shell stays strict; regrant with shell → strict next turn; regrant
without → still unverified; a main agent is unaffected). Live, 2026-09-23, deepseek2 main agent
and a deepseek2 worker granted `[read, edit]`: the worker changed `config.toml` and finished
`done` with `["none"]`; the host printed `· worker w1 (…/deepseek-v4.1-flash; read, edit,
finish) done — not verified; parent verification required` and `worker_result` carried the
same line — the ADR-0050 acceptance scenario that had ended `blocked` now ends honestly.
Run recorded in `docs/dogfood/runs.jsonl` (label `finish-policy`).
