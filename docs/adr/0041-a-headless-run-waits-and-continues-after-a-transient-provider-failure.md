---
adr: 41
title: A headless run waits and continues after a transient provider failure
status: proposed
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/completion.md, docs/dogfood/runs.jsonl, crates/p1-provider-http/src/retry.rs]
---
# ADR-0041: A headless run waits and continues after a transient provider failure

## Context

Unattended p1 jobs of 100+ requests run on subscription routes. On 2026-09-20 two of three such
jobs died on a transient provider failure: a chat stream that ended before its terminal event
after 14 requests, and an HTTP 429 after 145 requests, one step before the job's final gate. The
adapter layer (`p1-provider-http::drive`) retries only until a response starts and only for
seconds; once deltas were emitted it cannot retry transparently, and a quota window outlasts it.

## Decision

In headless runs the HOST treats `ProviderFailed` of kind `Transport` or `RateLimited` as a
transient turn end: it waits on a fixed schedule, then continues with one fixed user-role message;
bounded by `--provider-retries` consecutive failures (default 3), reset by any completed provider
response. Cancel wins during the wait. The core and the adapters do not change
(`docs/design/completion.md` §3b).

## Consequences

- A long job survives a dropped connection or a short quota pause; the interrupted response stays
  journalled as it is, and the retry is visible in the output and countable in the run report.
- The model sees one extra user message per retry; a response cut mid-tool-call is re-planned by
  the model rather than replayed by the harness.
- A run can now sit idle for up to ~21 minutes in total before it gives up on a rate limit.

## Alternatives considered

- Retry inside the core loop: the core would need time and a policy it has so far not needed;
  the host already owns the comparable continuation policy (ADR-0037).
- Retry inside the adapter after the stream began: partial output has already been emitted.
- Leave it to the operator (`--resume`): works, and is what unattended jobs cannot do.

## Evidence

`docs/dogfood/runs.jsonl` records `split3c-glm` (exit 1 on HTTP 429 after 145 requests); the run
`split3b` (2026-09-20) ended on `Transport: chat stream ended before [DONE]` after 14 requests
and completed after a manual resume. Issue #11.
