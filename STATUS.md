# Status

Handoff: `/home/phaseonebig/projects/phaseone-collab/fable-orchestrator-prompt.md`.
After any context compaction: re-read this file and `DECISIONS.md` first.
Branch: `slice-1` (integration). Started 2026-09-19.

## Done
- (0) Repo, workspace, `AGENTS.md`, gate script, core-isolation check, docs skeleton.

## In progress
- (1) Verify the two real routes (Anthropic subscription, OpenAI Codex subscription):
  request / tool-declaration / replay shapes; sanitized fixtures; Send-interface spike.

## Next
- (2) Contracts + agent core vs fake provider/tools, in-memory journal, ordering tests.
- (3) First provider adapter + read/edit/shell/search tools + headless host.
- (4) Second adapter + patch tool + environment assembly with prompt/config files.
- (5) JSONL journal, resume, interrupted-call reconciliation.
- (6) Optional delegation tool + in-process worker service.
- (7) Slice acceptance (seams.md §10), measurements, `docs/SLICE-REPORT.md`.

## Blocked
- nothing

## Worker runs
| When | Profile/effort | Task | Run dir | Result |
|---|---|---|---|---|
