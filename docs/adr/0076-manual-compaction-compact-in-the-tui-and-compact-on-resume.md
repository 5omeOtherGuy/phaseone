---
adr: 76
title: Manual compaction: /compact in the TUI and --compact on resume
status: proposed
date: 2026-09-25
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [issue #203]
---
# ADR-0076: Manual compaction: /compact in the TUI and --compact on resume

## Context

The owner ordered, 2026-09-25 17:26 (via p1-wasm-lead): "Tell p1-lead to implement
/compact command quickly in the meantime, so the fresh sessions starts with a working
/compact command." A long session read files in a loop for hours and its context was
spoiled, yet the summarizer (`p1-context`'s `SummarizingContext`, context.md §2) runs
only when the next request reaches `summarize_at_tokens`. The operator had no way to
compact now. Issue #203 asks for one entry point, the same summary and the same journal
record as the threshold trigger, no new crate and no new abstraction, and context
settings unchanged.

## Decision

1. **One summarization, two callers.** `SummarizingContext::summarize` is the single
   function that renders the history, asks the summarizer and builds the replacement.
   `prepare` (the threshold path) calls it once `summarize_at_tokens` is reached;
   `compact_now` calls it unconditionally. What a failure means stays the caller's:
   soft below the wall and fatal at it for `prepare`, an error for `compact_now`.
2. **The explicit entry point** is a provided method on the existing `ContextPolicy`
   trait, `compact_now(ContextInput) -> Result<Compaction, ContextError>`, with
   `Compaction::Replaced { prepared, tokens_before, tokens_after }` or
   `Compaction::Unchanged { tokens }` (estimated tokens, the policy's own estimator).
   The default refuses ("no summarizer"), so a passthrough environment without
   `[context]` says so instead of pretending. The agent owns its context behind
   `Arc<dyn ContextPolicy>` and a `/model` switch replaces it, so the method sits where
   the agent can reach whichever context is current.
3. **The identical record.** `Agent::compact_now(&mut self, cancel)` is the core's
   pass-through, callable only between turns like `Agent::reconfigure`. It installs the
   replacement through `install_replacement`, the one install the threshold path uses:
   validated, journalled as `ContextReplaced { items, usage }`, then the history, then
   the `ContextReplaced` event. There is no trigger field; a manual summary and a
   threshold summary of the same history are byte-for-byte the same record. A pending
   `Environment` is committed first, as a turn would. The last usage is forgotten after
   a replacement, so the next preparation estimates the new history instead of adding
   to a number that measured the old one.
4. **The no-op rule.** When no unit lies older than the tail `keep_recent_tokens`
   keeps verbatim, `compact_now` makes no request and returns `Unchanged`: what precedes
   such a tail is only the prelude — the task message, which the replacement keeps
   verbatim anyway, and an earlier summary — so a summary could only add to the
   history. The threshold path's own "nothing to summarize" is `Unchanged` too.
5. **TUI.** `/compact` queues exactly like `/model`: typed while a turn runs it says
   `compact queued · applies at the end of this turn` and waits for the turn's end;
   typed at idle it applies at once. The loop pumps the summary request like a turn
   (the screen stays live, ^C cancels it) and leaves one line of its own (the
   `ContextReplaced` event still renders its items row),
   `compacted: <before> → <after> tokens` or `nothing to compact: <tokens> tokens`
   (`compact failed: <reason>` on an error); `ctx` shows the new estimate. A known
   slash command typed while a turn runs is now a command, not steering text for the
   model (any other `/…` text stays steering). The command menu and `/help` list
   `/compact`; the C01 mock's `· 2 more` becomes `· 3 more`.
6. **CLI.** `--compact` belongs to the run grammar and needs `--resume`
   (`--compact needs --resume (a fresh session has nothing to compact)` otherwise, a
   usage error). With `--resume` the history is compacted once right after it is
   loaded and before the first provider request, printing the same line; the TUI
   queues it as a `/compact` so the line lands in its transcript. A headless or line
   run whose `--compact` fails stops with exit 1 instead of running on the old history.

## Consequences

- The operator can shrink a spoiled context on demand, in the TUI and when resuming.
- No context setting changed; windows, effort and the summary output cap are the ones
  the threshold path uses.
- `ContextPolicy` gained a provided method. Existing implementors are untouched; a
  policy that can summarize overrides it.
- A manual compaction counts as one summary for the headless stall guard (§3c), the
  same as a threshold summary: it is one.
- `/model` and the other known commands typed mid-turn now run as commands (the switch
  queues for the boundary, as §11 always described) instead of reaching the model.

## Alternatives considered

- A concrete `Arc<SummarizingContext>` kept by the host beside the agent: every
  assembly and `/model` switch would have to thread a second handle, and the core would
  need a generic "replace history" entry any caller could misuse. Rejected.
- Lowering `summarize_at_tokens` for one turn to force the threshold path: it changes a
  context setting and needs a turn. Rejected.
- A steering verb for headless runs: the issue rules it out.

## Evidence

- `crates/p1-context/tests/compact_now.rs`: one summary and the counts, the no-op, the
  byte-for-byte record against the threshold path.
- `crates/p1-host/src/tui/tests.rs`: `a_compact_typed_mid_turn_applies_at_the_turns_end`,
  `a_compact_at_idle_applies_at_once`.
- `crates/p1-host/tests/manual_compaction.rs`: `--compact --resume` summarizes before
  the first request; `--compact` without `--resume` is refused.
- `crates/p1-tui/src/render/picker.rs`: `the_command_menu_lists_compact`.
