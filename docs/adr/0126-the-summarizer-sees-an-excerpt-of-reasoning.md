---
adr: 126
title: The summarizer sees an excerpt of reasoning
status: accepted
date: 2026-10-08
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0126: The summarizer sees an excerpt of reasoning

## Context

Issue #421. The context summarizer renders the history it replaces as a transcript, and that transcript omits reasoning text (`crates/p1-context/src/render.rs`, `docs/design/context.md` "Rendering"; the frozen acceptance test `crates/p1-context/tests/acceptance_sol.rs` asserts the omission). For DeepSeek, 93 % of output is reasoning and the visible text is nearly empty, so the summary records what the model did but not what it concluded. On W0 the summary at seq 263 said "No defect has been confirmed" and "Next step: Read commit.rs ≈370–830", and the model then redid the review: 45 of 47 post-summary file reads were re-reads, 183 of 231 across 7 summarized DeepSeek runs, about 329 s on W0.

The owner decided on 2026-10-07, in the question dialog, to revise the frozen assertion so the summarizer gets a bounded excerpt of reasoning, rather than adding a model-written handoff step.

## Decision

The summarizer sees an excerpt of each summarized assistant item's reasoning.

1. **Rendering.** Inside an `## Assistant` block, each reasoning block with non-empty text is rendered, in the item's block order, as `Reasoning (excerpt): ` followed by its text cut to `reasoning_excerpt_chars` by the same head-and-tail excerpt the tool results use (`[… n chars omitted …]` between). It counts toward the block's text limit like a text block. No new heading is added. Replay data is never rendered.
2. **Setting.** `reasoning_excerpt_chars` joins the `[context]` table (default 4,000; 0 restores the old omission and is therefore valid, unlike `tool_result_excerpt_chars`). It is threaded wherever `tool_result_excerpt_chars` is: `ContextConfig`, the context component's accepted keys, the host's context table and `ContextSettings`.
3. **Budget.** The transcript keeps its existing budget; when it is over, the oldest rendered items are dropped first, as today, so the newest reasoning survives.
4. **Prompt.** The default summarizer prompt asks, under "State of the work", for the working conclusions and candidate findings the assistant reached, including those that appear only in its reasoning.
5. **Spec and frozen test.** `docs/design/context.md` "Rendering" says reasoning text is rendered as an excerpt. The frozen acceptance test's assertion that the reasoning text is absent becomes an assertion that its excerpt is present inside the `## Assistant` block; its heading count and order are unchanged. This frozen expectation changes by the owner's decision above. A second frozen edit, approved by the owner on 2026-10-08, raises that same test's `window_tokens` by 1,000: the excerpt line counts against the transcript budget, which the test sized to the old render, so without the room the oldest items were dropped and the heading count failed. No other frozen expectation changes.

## Consequences

- Summaries can carry conclusions reached in reasoning, so a model resumed from a summary has fewer reasons to re-read what it already reviewed.
- The summarizer's input grows by up to 4,000 characters per summarized assistant item, inside the same budget.
- Reasoning text, which can quote file contents, now reaches the summarizer; the summary is still secret-masked before it is stored.

## Alternatives considered

- **A model-written handoff note before each summary.** Rejected by the owner's decision: a larger change in the core loop and one more model request per summary.
- **Render reasoning only for the last N items.** Rejected: the transcript budget already keeps the newest items when it must drop any.

## Evidence

Issue #421; analyst report `~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md` (W0 summary seq 263, re-read counts). Code: `crates/p1-context/src/render.rs` (reasoning omitted), `crates/p1-context/src/lib.rs` (config, default prompt), `modules/p1-module-context/src/settings.rs`, `crates/p1-host/src/summary.rs`, `crates/p1-assembly/src/lib.rs` `ContextSettings`.
