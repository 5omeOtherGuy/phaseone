---
adr: 125
title: Read takes several files or ranges in one call
status: accepted
date: 2026-10-08
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0125: Read takes several files or ranges in one call

## Context

Issue #418, rescoped on 2026-09-27 after the analyst's measurement. Each DeepSeek request in p1 costs at least 1.1 s plus 2.16 s per 100k tokens of context before it generates anything. W0 made 135 requests at up to 297k context, 344 s of pure context latency. Parallel tool calls are not the lever: DeepSeek made 1.19 calls per turn in p1 and 1.14 in pi. Content per round trip is: GPT-6 Sol read W0 in 12 requests by concatenating files, about 15k characters per result, while DeepSeek read one file range per request.

`read` takes one `file_path` with `offset`, `limit` and `skim`, and returns at most 2,000 lines and 50,000 bytes (`crates/p1-tool-read/guest/src/lib.rs`). The guest crate holds the schema, parsing and rendering for both the native adapter and the `p1/read` component.

## Decision

`read` reads several files or ranges in one call.

1. **Input.** `read` gains `files`: an array of 1 to 10 entries, each `{file_path, offset?, limit?, skim?}` with the same meaning and defaults as the single form. A call gives either `file_path` (with its own `offset`, `limit`, `skim`) or `files`, never both; both or neither is an input error naming the two forms.
2. **Output.** One section per entry, in the order given, each headed `==> <file_path> <==` and rendered exactly as a single read of that entry renders, including its next-offset line. The whole result keeps the single read's 50,000-byte cap: when it is reached, every later entry gets the line `not read: this call's output limit was reached; read it in another call`, so nothing is silently dropped.
3. **Errors per entry.** A missing, refused or binary file gets its own error line in its section; the other entries are still read. The call's status is an error only when every entry failed.
4. **Read state.** Each entry records its observation exactly as a single read of that entry does, so read-before-write rules hold per file.
5. **Prompts.** The `read` description says to put several files or ranges into one call with `files` rather than one call each. The review prompt (ADR-0124) tells the reviewer to batch its reads that way.

## Consequences

- A model that follows the description reads a set of small files, or several ranges of one file, in one request instead of one request each, which cuts requests and the context latency each one pays.
- Results grow per call up to the existing 50,000-byte cap; the cap itself is unchanged.
- Both read adapters (native and component) get the form through the shared guest crate; the WIT is unchanged because `read`'s input is JSON.

## Alternatives considered

- **Parallel tool calls.** Rejected by the measurement above: models already issue about one call per turn whatever the harness allows.
- **A larger output cap for one file.** Rejected: a single 2,000-line window is already larger than what the models chose to read; the waste is the number of round trips, not the window.
- **A separate `read_many` tool.** Rejected: one more tool schema in every request, for a form `read` can take.

## Evidence

Issue #418 and the analyst report `~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md` (requests per run, context latency model, Sol's read pattern). Code: `crates/p1-tool-read/guest/src/lib.rs` (schema, caps, render), `crates/p1-tool-read/src/lib.rs` (native adapter), `modules/p1-module-read`.
