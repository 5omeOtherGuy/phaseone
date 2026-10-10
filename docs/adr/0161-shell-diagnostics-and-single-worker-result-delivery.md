---
adr: 161
title: Shell diagnostics and single worker-result delivery
status: accepted
date: 2026-10-10
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0161: Shell diagnostics and single worker-result delivery

## Context

Issue #82 records commands rerun because summaries hid diagnostics, and a worker
result delivered twice: once by a waiting `worker_result`, then by an inbox notice.
Three frozen expectations conflict with the requested behavior. Following ADR-0126,
the lead deferred implementation rather than changing those expectations without
an owner decision.

The owner approved exactly the three changes on 2026-10-10 in
[the issue decision](https://github.com/5omeOtherGuy/phaseone/issues/82#issuecomment-6101653326),
referencing [the conflict list](https://github.com/5omeOtherGuy/phaseone/issues/82#issuecomment-6101298484).
The recorded decision says: "Owner decision 2026-10-10: approved. #82 may change
the three frozen expectations" and "No other frozen expectation changes."

Pre-push then exposed additional old-render expectations in the declarative tier,
including the frozen jq-redirection one. The owner extended the approval on
2026-10-10 in [the follow-up decision](https://github.com/5omeOtherGuy/phaseone/issues/82#issuecomment-6101966709):
"#82 may also update the expectations and fixtures in
crates/p1-tool-shell/tests/declarative_filters.rs" listed in
[the second conflict list](https://github.com/5omeOtherGuy/phaseone/issues/82#issuecomment-6101830737),
"including the frozen jq-redirection expectation", and "fixtures too small to
exercise filtering may be lengthened so they still do."

## Decision

1. **Shell diagnostics.** After any recognized filter applies, retain the first
   80 original lines matching `error`, `warning`, `panic`, or `failures`
   (case-insensitive), or literal `FAILED`. Keep each line verbatim, including
   repeated occurrences; recover lines a filter removed without duplicating those
   it retained. Keep the filter's other failure detail and ordering where possible.
   The existing head/tail byte limit remains the final backstop.
2. **Raw hint.** End the summary body with `raw: true for the full N lines`, where
   N counts the captured input lines. The existing filter marker, output-recovery
   framing and exit footer follow it. Capture-loss warnings still survive; the hint
   does not promise recovery of bytes already dropped by capture. Raw bypass and
   never-larger fallback are unchanged. Successful cargo-test and vitest summaries
   omit timing fields to make room for the hint while preserving the existing
   corpus reduction bars and result counts.
3. **Worker delivery.** `WorkerService::wait_result` distinguishes an observer
   receiving a result from an internal lifecycle wait. Both scoped and native
   observer adapters use it. When a child ends while result waits are pending,
   defer its notice: successful terminal delivery suppresses it; cancellation or
   dropping the last waiter without delivery sends it. Each continuation has its
   own receipt and retained terminal result, so a continuation cannot replace
   the result an older wait receives. Idle-parent notices, lifecycle waits and retained repeatable
   results are unchanged. Notices already queued before a result wait began are
   not retracted.
4. **Frozen expectations, owner-approved only.** Change exactly:
   - `crates/p1-tool-delegate/tests/delegation.rs::child_finishing_while_parent_waits_in_worker_result`:
     no duplicate inbox notice after the waiting result is returned.
   - `crates/p1-tool-shell/tests/output_filters.rs::a_filtered_result_ends_with_the_marker_line`:
     the clean summary includes the N-line raw hint before existing framing.
   - `crates/p1-tool-shell/tests/output_filters.rs::the_byte_bound_stays_the_backstop_after_the_filter`:
     the first 80 diagnostics survive, later ones do not; both capped fixtures fit
     before the byte backstop and report their full captured line counts.
   - The eight tests named in the second conflict list in
     `crates/p1-tool-shell/tests/declarative_filters.rs`:
     `a_declarative_filter_summarises_a_recognised_tool_class`,
     `a_declarative_filter_dispatches_through_shell_plumbing`,
     `a_declarative_filter_emptying_output_yields_raw_or_on_empty`,
     `a_failing_recursive_make_keeps_its_error_lines`,
     `structured_filters_keep_precedence_over_declarative_filters`,
     `gradle_and_gcc_commands_select_their_filters`,
     `an_ambiguous_shell_shape_keeps_the_raw_output`, and
     `a_redirection_right_after_a_name_selects_its_filter` (frozen jq-redirection):
     exact summaries include the N-line hint and recovered diagnostics. Lengthen
     helm blank chatter, make directory chatter and gcc include chatter where
     necessary so fixture summaries remain smaller than raw including notices.
     Raw-bypass expectations and the jq fixture itself remain unchanged.
   No other frozen expectation or corpus fixture changes.

## Consequences

- The model sees diagnostics without rerunning a command and gets a concrete raw
  escape hatch. Large diagnostic sets remain bounded.
- A pending worker-result wait and the inbox do not deliver the same completion
  twice; abandoning a wait does not lose the completion notice.
- Existing corpus reduction, raw passthrough, capture-loss and byte-bound
  guarantees still apply. Timings remain available in raw output.

## Alternatives considered

- **Change frozen tests without approval.** Rejected per ADR-0126; the owner
  approved only the original three and the named declarative expectations/fixtures.
- **Suppress every notice whenever any wait starts.** Rejected: canceled or
  dropped waits have not delivered a result, and lifecycle waits still need notices.
- **Weaken reduction bars to fit the hint.** Rejected: shorten successful test
  summaries instead, preserving counts and diagnostic detail.

## Evidence

Issue #82 and its linked approval and conflict-list comments above. Frozen suites named in the decision;
`p1-shell-guest` exercises diagnostic recovery across all five match classes and
the 80-line boundary; `p1-workers` exercises canceled and dropped waits, concurrent
waiters and continuation notices through the public service interface.
