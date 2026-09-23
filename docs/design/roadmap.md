# p1 roadmap (2026-09-23)

Where p1 stands after the first three weeks, what is still missing, and the order of the next
work. Every item points at a GitHub issue written to one template: **current state** (verified
at a named commit, with file and line), **implementation**, **behaviour tests**, **definition of
done** (measurable), **state after**. The epic issue #72 tracks this list; `STATUS.md` "Next"
stays the lead's short-form pointer.

## Usable today (main `b7f16b6`, CI green)

- Interactive TUI (SLAB, ADR-0056) with model picker, call descriptions and edit previews
  (ADR-0057/0059), interactive stall warning (handoff §14.7).
- Three provider families behind route and profile files (ADR-0039), `p1 login` and a
  permission-checked credential store (ADR-0044).
- Headless runs for workers (`scripts/fanout.py`, `runner: p1`), the `finish` tool with honest
  "done — not verified" reporting (ADR-0051), role fallback chains (ADR-0054), the stall guard
  with a workspace fingerprint (ADR-0042/0055).
- Workflows in the Claude Code style: `p1 workflow run` with rhai scripts, roles, caps, journal
  and resume (ADR-0053).
- Brain shadow hook (ADR-0058), usage ledger (ADR-0052).

Evidence: 76 dogfood journals in `../phaseone-briefs/runs/`, 70 accepted runs in
`docs/dogfood/runs.jsonl`; every worker of 2026-09-21…23 ran inside p1.

## Gaps (measured or observed)

| gap | evidence | issue |
|---|---|---|
| Each worktree needs 8–15 GB of build artifacts; a stopped rustc stalls every build | 2026-09-23 disk and stall incidents, ADR-0060 | #61 |
| Interactive sessions are not journaled unless `--session` is given; no session list, no resume by id | `crates/p1-host/src/run.rs:956` | #62 |
| Journals carry no time: ~20 s Enter-to-first-text cannot be attributed | perf audit §3, 0 of 76 journals have a timestamp | #63 |
| Usage accounting was a one-off script and produced two errors | perf audit, Astra review items 1 and 9 | #64 |
| The TUI draws unconditionally on 50 ms ticks; frame cost unmeasured | `crates/p1-host/src/tui.rs:1107`, audit §4 | #65 |
| Compaction requests are fully uncached on non-Anthropic routes (33.7 k mean, 104 k on DeepSeek) | audit §2 | #66 |
| A panicking child build under the workers' mutex takes the service down | 31 `lock().unwrap()` in `p1-workers` | #67 |
| Worker queueing, per-worker usage and context parts are invisible to the operator | handoff §14.3, §14.10 | #68 |
| Changes made through the shell have no diff in the TUI | handoff §14.4 | #69 |
| 147 duplicated lines edit↔write; a cross-crate test include; two dead-code allowances | audit §5 | #70 |
| Seven TUI decisions only the owner can make | handoff §15 | #71 |

## Order

Owner priority (2026-09-23 23:30): ADR-0060 first. Then the measurement chain (timing before
anything that claims a latency gain), then the rest.

1. **#61 ADR-0060** — salt spike as the first DeepSeek job after the reset, then the scripts and
   profiles. Astra's audit of the ADR, spike protocol and 48-hour plan land in
   `~/scratch/p1-next/PLAN.md` (XO, 2026-09-23); the lead reads it first.
2. **#63 timing instrumentation** (ADR) and **#64 usage-audit script** — independent, run in
   parallel.
3. **#62 session history** — the gap the owner asked about; small, host-only.
4. **#65 TUI draw suppression** and **#67 workers critical sections** — independent.
5. **#68 seams batch 2**, then **#69 diff seam** — with the TUI session on #12.
6. **#66 compaction experiment** — after #63, because its A/B needs the timing marks.
7. **#70 hygiene** — any idle worker slot.
8. **#71** — owner answers whenever convenient; nothing blocks on it except the `/resume` picker.

Routing (owner/XO 2026-09-23): DeepSeek V4.1 Flash on the primary Go subscription is the
worker; verification protocols by Sol; Claude-side reviews by Fable; Astra consults, never works.
The operational hold stands until Thursday 2026-09-25 19:00.

## Session-log scan (2026-09-23, in progress)

Seven DeepSeek workers are scanning the 76 journals for tool failures, wasted work, provider
trouble, compaction losses, finish honesty, instruction following, usage anomalies and harness
ergonomics (brief: `../phaseone-logscan/BRIEF.md`). Their findings become issues in the same
template and are appended below with the date.

## Not on the list (on purpose)

- #45 re-reading after a context summary — on hold by the owner.
- #25 fan-out program — owner-owned; nothing active.
- #6 dogfooding group — no harness debt left from its list.
