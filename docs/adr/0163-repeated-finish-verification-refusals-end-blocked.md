---
adr: 163
title: Repeated finish verification refusals end blocked
status: accepted
date: 2026-10-10
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/adr/0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md, docs/design/completion.md, crates/p1-tool-finish/guest/src/lib.rs, crates/p1-host/src/activity.rs]
---
# ADR-0163: Repeated finish verification refusals end blocked

## Context

Issue #86 reports a monitor task repeatedly refused at finish. The gate compares command
identity, exit status and freshness, not output bytes; a fixture reproduces the loop when
a changing snapshot's later state write makes its check stale on every retry.

The issue requested an unverified done fallback. The owner decided on 2026-10-10,
recorded on #86: "after the bounded number of refused finish calls the run ends `blocked`
with the refusal reason and what would satisfy it; invalid evidence never downgrades to
done (ADR-0051 §2 stands)." This extends bounded completion without reversing ADR-0051.

## Decision

1. Three consecutive verification refusals for the same failed checks end as blocked.
   The first two retain their existing errors. The third carries the caller's summary,
   the latest refusal and remedy in needs, and the named checks in tried. No structured
   result or done evidence is committed. The result description says blocked.
2. The key is the sorted, deduplicated failed-check spellings with whitespace collapsed.
   Directory qualifiers and expected-exit suffixes remain. Passing commands do not change
   it. Missing/empty verification and none-after-write are counted as well.
3. Different failed checks restart the sequence. Accepted completion, malformed input,
   and non-verification errors reset it. Intervening checks and writes do not reset it:
   those are precisely how a monitor can keep retrying. Each agent owns its counter.
4. The native adapter and production completion gate share the bound logic in
   p1-finish-guest. The host owns the component counter across re-grants and submits only
   a blocked candidate through its existing completion hub. Stale grants cannot advance
   the counter or commit a fallback. No WIT, policy or evidence type changes.
5. ADR-0051 §2 is unchanged: invalid evidence never becomes done. Its legitimate
   ReportToParent none outcome and strict-policy none-without-writes outcome stay valid.

## Consequences

A recurring verification refusal cannot trap an agent beyond three attempts for the same
failed checks. A caller receives an honest blocker and the action needed to establish
evidence. A recoverable check gets two retries; changing to a different check starts a new
allowance. This is a finish-specific bound, not a general repeated-tool reminder.

## Alternatives considered

- Unverified done: contradicts ADR-0051 and falsely reports successful completion.
- A native-only counter: production uses a component and the host independently verifies it.
- A component-local counter: component calls are isolated and components are not an
  authority for the host's completion state.
- Reset after every check or write: leaves the monitor retry loop unbounded.

## Evidence

The changing-snapshot regression fails at the third refusal on the unchanged native adapter
and production boundary, then passes with the bound. Re-run with:

- cargo test -p p1-tool-finish -p p1-finish-guest
- scripts/build-modules.sh --all
- cargo test -p p1-module-tests --test finish_boundary

The tests check blocked rather than done, remedy text, ends_turn, no structured result,
blocked display without a checkmark, re-grants, agent isolation, stale grants, fabricated
not-run evidence, different checks and successful-completion reset. A control accepts
different snapshot output when its successful check remains current.
