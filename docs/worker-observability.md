# Worker observability programme

Read `AGENTS.md` and the global owner orders.
Deliver all seven owner requirements from 2026-09-24 09:31:
1. Match pi's telemetry quality.
2. Dogfood p1 for workers and fix every encountered defect.
3. Analyze historical cost, time and quality.
4. Monitor new p1 cost, performance and time continuously.
5. Make evidence digestible and browsable.
6. Make everything accessible from the terminal.
7. Keep dashboard and analytics separate modules under p1 modularity.

Keep the quota dashboard and its repair; make worker performance a separate composable module.
Deliver CLI tables, terminal charts, keyboard navigation and machine exports; no browser/React/Sites dependency.
Do not call the programme complete after only design or handoff.
The p1 lead owns delivery: workers implement, a different worker or model reviews independently, and the lead integrates and monitors.
Start historical analysis independently of route/UI implementation.

## Architecture

Read current accepted ADRs and the modularity audit before proposing interfaces.
Keep ingestion, storage, analytics and views behind separate interfaces in optional crates.
Use ordinary constructor composition and minimal host glue.
Apply the project's issue, ADR, ownership and exact-commit gate rules.
Have the lead review proposed names and boundaries; do not impose architecture through an XO notice.

## Telemetry

Record durable UTC start/end and monotonic durations.
Record run, task, attempt, session and parent IDs, project, harness version and binary hash.
Record requested and actual model, route and effort.
Record request-level uncached input, cache-read, cache-write, output and reasoning usage with provider semantics.
Do not add reasoning to output when it is already a subset.
Record tool calls, outcomes, errors and retries as metadata in run records; credential values, private prompts and raw authenticated traffic never enter them.
Record finish, interrupt, abandon and error states.
Separate compaction/summary and child usage.
Deduplicate resumes, recover crash tails and support rotating/incremental retention.
Exclude prompts, secrets and tool contents from analytics exports.
Use None for unknown values.
Separate observed cash, model-rate estimates and subscription quota percentage points.
Record cost provenance, resets, overlap and coarse measurement precision; do not invent subscription dollars.

## Quality and history

Derive quality from independent acceptance, review, repair and rejection evidence.
Track missing or unmatched evidence; never infer quality from exit 0.
Separate per-attempt execution from final task outcome and repair count.
Join historical records exactly, or label uncertain linkage explicitly.
Do not invent fuzzy task/model matches.
Report cohort n, coverage, task mix, model, harness and time.
Do not claim causal model rankings from heterogeneous tasks.
Measure production free-share among eligible new dispatches and show reasons for exceptions.
Use `~/.agents/skills/model-cards/SKILL.md` for per-result records and research-lead proposals.

## Terminal and monitor

Provide overview, sortable/filterable run table and run drilldown with provenance.
Filter by date, project, harness, model, effort and status.
Label tokens, cash, estimates, quota percentage points, time and quality separately.
Show unknown, stale and partial states.
Support bounded refresh, resize and keyboard navigation.
Provide plain, JSON and CSV output when not attached to a TTY.
Show medians, tails and cost/time per verified accepted task only where evidence supports them.
Display measurement coverage.
Ingest new, completed and crashed runs incrementally.
Display freshness, errors and lag.
Run monitoring independently of a lead session, with no model calls or quota API hammering.
Avoid duplication after restart, reconnect or replay.

## Acceptance

Verify integrated telemetry feeds the same analytics contracts.
Verify historical backfill against independent reconciliation.
Run meaningful frozen and edge-case tests.
Exercise real terminal interaction, refresh and resize.
Leave a persistent monitor active.
Deliver the verified user command, module/dependency evidence and concrete dogfood defects/resolutions.
Use read-only scans/scripts without unnecessary Rust rebuilds.
Keep resource limits and build placement from the global instructions.
Historical source: `~/.agents/archive/P1-WORKERS-ONLY-history.md`.
