---
adr: 127
title: The context policy shortens old tool results before it summarizes
status: accepted
date: 2026-10-08
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0127: The context policy shortens old tool results before it summarizes

## Context

Issue #419, rescoped on 2026-09-27. p1 let W0's context reach 297k tokens before it summarized; turns 71-114 ran 1.5 times slower than pi's latency model predicts, and pi, which stayed at or below 215k, never entered that regime. Reasoning replay must stay inside a tool loop (one user turn), so the lever is old tool results.

Nothing trims old tool results on ordinary requests today: `tool_result_excerpt_chars` shapes only the summarizer's transcript (`crates/p1-context/src/render.rs`). The context policy's `prepare` runs before every request (`crates/p1-core/src/lib.rs`); a `Prepared` it returns replaces the history and is journalled as `ContextReplaced`. Shortening a tool result's content keeps call and result paired, which `validate_replacement` requires.

## Decision

Before it summarizes, the context policy shortens old tool results.

1. **Trim threshold.** The `[context]` table gains optional `trim_at_tokens`, below `summarize_at_tokens`. When the next request's estimated input (the same figure the summary threshold uses) reaches it, the policy replaces the `content` of every tool result older than the kept tail (`keep_recent_tokens`, the same tail the summary keeps) and longer than `tool_result_excerpt_chars` with its head-and-tail excerpt at `tool_result_excerpt_chars`, followed by the line `[older result shortened by p1; read the file or rerun the command for the full text]`. Results already ending in that line are left alone. When no result changes, the history is sent unchanged.
2. **No summary when the trim is enough.** If the trimmed history's estimate is below `summarize_at_tokens`, the policy returns it and does not summarize; otherwise it summarizes the untrimmed history exactly as today (the summary transcript excerpts tool results itself), so a summary never sees a twice-cut result.
3. **Journal.** A trim is a context replacement like a summary: journalled, resumed and validated the same way. Nothing else in the history changes: user items, assistant items, reasoning and the tail stay byte-exact.
4. **Shipped values.** The DeepSeek environments (`deepseek`, `deepseek1`-`deepseek3`, `deepseek-review`, and `cline`, `cline2`, which run the same model) set `trim_at_tokens = 150000`, with `summarize_at_tokens` unchanged. LEAD POLICY, not a measurement: it leaves 50k of the issue's 200k target for the tail and growth between requests; the replay round measures it.

## Consequences

- Long reviews keep their working set and lose only the bulk of old tool output, which the model can fetch again; most runs should stay below the summary threshold.
- Each trim is one more journal record and breaks the prompt cache from the first changed result onward; while the input stays above the threshold, results that leave the tail are shortened request by request, near the end of the history, so most of the prefix stays cached.
- An environment without `trim_at_tokens` behaves exactly as today.

## Alternatives considered

- **A view-only projection per request, outside the journal.** Rejected for now: it needs a new seam in the core between history and request; the existing replacement path already handles journalling and resume.
- **Trim reasoning instead.** Rejected: reasoning replay within a tool loop is required by the routes that use it.
- **A lower summary threshold.** Rejected: it makes summaries, and the re-reads #421 measured after them, more frequent.

## Evidence

Issue #419; analyst report `~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md` (W0 context curve, pi's peak). Code: `crates/p1-context/src/engine.rs` (`prepare`, threshold, `accept`), `crates/p1-context/src/plan.rs` (tail), `crates/p1-context/src/render.rs` (`excerpt`), `crates/p1-core/src/lib.rs` (`install_replacement`, `validate_replacement`).
