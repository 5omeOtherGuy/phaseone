# Iris scale workflow

Deliver the owner-requested approximately 50-invocation p1 workflow for the entire TUI port under programme #91 and its disposition manifest.
Treat 50 as total invocations across phases, not simultaneous workers.
Keep live-TUI deployment subject to the owner's decision.

## Prepare

Finish the current #93 slice and wf6 findings.
Freeze the provisional 53-item disposition manifest into slices with owned files, acceptance cases and dependencies.
Obtain the dashboard lead's agreement on shared UI seams; those files remain its responsibility.
Write explicit Rhai for one workflow_start: implement, independently verify, review, repair, integrate, finalize.
Dry-run 2–3 slices on DeepSeek to validate the script and telemetry.
Write phase counts, wall-clock estimate and per-route quota estimates in `unified-dashboard-trial/iris-massive-workflow-PLAN.md`.
Use one worktree per task, with sequential implement/repair writers and explicit file ownership.

## Start conditions

Keep preparing and progressing the current slice until the trigger is met.
Require route registration and live verification in both the default p1 store and the Iris trial's XDG_CONFIG_HOME.
Verify `opencode-zen-1/2/3`, `cline-pass-1/2` and `opencode-go-1/2/3`, with the permitted served models.
The routes shipped in #107 (b3d0464); the p1 lead owns them and keeps the trial copies in sync.
Have XO verify `p1 login --list` and one live request per route.
Require PR #107 merged and the resulting routes present in the trial configuration.
Require the completed plan and XO approval/start under the owner's 22:45 delegation.
The owner's 22:45 order (via XO) settles the roles in §Roles; the Opus 5.5 medium judge is an authorized in-workflow role, not a new lead.
Confirm the data scope before free models read non-public files (free providers may log prompts); do not silently expand permissions.

## Roles

Use Space Bunny as the current implementation worker.
Use MiMo/Muse only after the required client-identity route fixes and live verification.
Spread eligible free work across Zen accounts.
Use DeepSeek/GLM on Go and Cline Pass for review, verification and repair within each route's permitted models.
Move a failed free-model task to DeepSeek after two failed attempts.
Use Opus 5.5 medium for workflow judgment as ordered at 22:45; free models make no acceptance or design decisions.
Do not use Fable, Sol or Kimi as workers in this workflow.
Keep verification independent of implementation by a different model or at least a separately briefed role using frozen cases.
Have the authorized judge/lead accept each slice from evidence, never exit status.

## Execution and resources

Use the trial's `max_threads=8`; raise `max_steps=200` only as needed for the planned workflow.
Keep engine thread settings subject to global worker/build caps and resolve GLM accounting before exceeding a pool.
Follow global HDD targets, two-job Rust settings and SSD floor; watch memory with `free -m`.
Use dependency barriers only where needed and supervise nonblocking under model-cards.
Reroute a route-level 429 to another permitted account/model within verified admission; never use a frontier subscription model as fallback.
Have XO monitor `~/.agents/xo/quota-summary.txt`.

## Evidence and landing

Record each worker's actual model, route, effort, elapsed time, tokens, tool calls/failures, provider errors and route-specific 429s.
Record measurable quota percentage points and cash provenance separately; unknown is not zero.
Record every p1 defect with a reproducer.
Adapt donor tests and preserve frozen acceptance cases.
Finalize docs, STATUS, ADRs, changelog and the full guarded gate.
Obtain independent review of the integrated tree before merge.
Follow global PR/review/repair/merge ownership; retain the separate owner decision for installing the live TUI.
Historical source: `~/.agents/archive/OWNER-ORDER-iris-50-worker-workflow-20260924-history.md`.
