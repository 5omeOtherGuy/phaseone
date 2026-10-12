# Session-log scan — 2026-09-23

Owner order: scan the dogfood session journals for signs of issues and options for improvement.
Seven DeepSeek V4.1 Flash workers (`pi-worker deepseek`, effort high, read-only) each took one
shard of the 62 run directories in `../phaseone-briefs/runs/` (76 journals, 60 runs scanned, 2
skipped as live or journal-less). Brief and raw outputs: `../phaseone-logscan/` (`BRIEF.md`,
`findings-s1..s7.json`, `notes-s1..s7.md`). 99 findings; each names run directory and journal
`seq` numbers. Verified by the lead: every finding used below was checked against main
`dfaa6de` before an issue was written; findings from 2026-09-20 runs that predate ADR-0041
(turn retry), ADR-0042/0055 (stall guard + fingerprint) and #54 (finish always) are marked.

The one "possible secret" report (s4-01) is a false positive: the pattern matched inside ADR
filenames (`…task-…` → `sk-…`) and the repository's own sentinel fixtures. Nothing was copied.

## Findings → issues

| theme | findings (shard-id) | runs | issue |
|---|---|---|---|
| finish rejects `done` because the check ran in a chain or pipe; whole suites re-run | S3-01 s6-03 s4-03 s5-04 s2-08 s7-04 s4-02 s2-10 | 7 of 10 in one shard, 6 of 8 in another | #73 |
| edit refused as unread/stale, incl. after the model's own edit; O(n) re-reads | s4-08 s4-07 S3-06 s4-11 s2-11 s7-02 s5-09 s6-08 | ≥ 6 | #74 |
| argument bounds only in the schema (`context` ≤ 10 rejected 19×); unknown-field errors; unusable env offered | s1-01 S3-04 s6-07 s7-03 s2-04 s4-13 s5-14 s5-08 s7-05 s7-06 | ≥ 10 | #75 |
| `read` refuses outside-workspace files the brief names; shell reads them | s7-01 s6-05 S3-05 s2-09 s5-07 | ≥ 6 | #76 |
| shell timeouts on cargo (10 200 s lost), parallel cargo deadlock (2 × 3 600 s), gate over the cap | s6-01 s2-01 s5-13 s4-16 | 4 | #77 |
| runs end without finish on provider failures; interrupted records lack usage; startup aborts and SIGTERM unjournaled | s1-07* s2-06 s5-01 s6-02 s7-11 s4-05 s4-06 s6-11 s5-03 s5-12 | 9 | #78 |
| summaries garbage or truncated; resume re-summarizes; compaction storm | s1-09 s2-05 S3-12 s1-08* | 3 | #79 |
| `cache_write` null on chat routes; Claude route lacks reasoning/cost; uncached re-sends without compaction | s4-15 s1-11 s7-14 s6-12 s2-12 s4-09 S3-08 s6-10 s7-13 S3-07 s5-11 | ≥ 12 | #80 |
| git exit 128 inside sandboxed worktrees | s2-02 s7-18 | 3 | #81 |
| shell summariser hides failures; duplicate worker inbox notice | s7-17 s7-19 | 10 | #82 |
| re-reading files right after a compaction (summary dropped their content) | S3-02 s4-04 s5-05 s6-09 s7-09 s1-05 s1-03 s7-08 | every shard | #45 (owner hold; evidence added) |
| no-progress loop: 879 calls, zero mutations, 40 compactions (2026-09-20) | s1-04* s1-06* s1-10* | 1 | covered by ADR-0042/0055 and #54 since; no issue |
| stall guard cancelled a run editing through heredocs | S3-03* | 1 | fixed by ADR-0055 (#53) |
| ws-continuation injected after HTTP 400 ×3 | s4-05 | 1 | in #78's matrix |

`*` = run predates the fix named in the same row.

## Brief-side lessons (no code change)

- Briefs named files absent from the run's worktree (s7-15) and one brief had no workspace root
  (s4-12): always merge `main` into the worktree before dispatch and name paths that exist there.
- Two runs edited files outside their owned paths and disclosed it (S3-09, s5-10): keep the
  disclosure rule; the lead reviews the diff anyway.
- No run's `report.json` records independent acceptance (`accepted: unknown`, s7-16): the lead
  sets it when recording the run in `docs/dogfood/runs.jsonl`.
- `report.json` schema drift across runs (s2-13): re-run `scripts/run-report.py` before comparing
  old runs.

## Not measurable from journals

Latency (no timestamps, #63), why a compaction fired (not journaled), and cost on routes that do
not report `cache_write` (#80).
