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

CI then exposed module parity expectations for short logs whose new summaries
correctly fall back to raw. The owner approved "82: A" on 2026-10-10 in
[the consolidated decision](https://github.com/5omeOtherGuy/phaseone/issues/82#issuecomment-6102327864):
every filter expectation and fixture changed only by the approved N-line hint,
diagnostic retention and notice-inclusive shrinking may change in the six files
listed below, frozen ones included. Small fixtures may be lengthened to exercise
filtering. One repair push is authorized; nothing else frozen changes.

The host stall-guard acceptance test independently required the same duplicate
notice as the delegation test. The owner approved "82 stall A" on 2026-10-10 in
[the stall-guard decision](https://github.com/5omeOtherGuy/phaseone/issues/82#issuecomment-6102466407):
change only its duplicate-notice assertion to require absence after a waiting
`worker_result` delivered the failure. Nothing else in that file changes.
The dispatcher clarified that approval on the same date: the assertion checks
history order, allowing a legitimate notice before the result, but forbidding a
notice after the result delivered the failure. The unsynchronized fixture can
complete before its wait begins; an assertion of unconditional absence would
incorrectly reject that existing behavior. That clarification initially authorized
no fixture or implementation changes.

Validation showed that a notice queued before the wait could still be drained
after the result and therefore duplicate the failure for the model. The
dispatcher then expanded the scope on 2026-10-10 to withdraw such an undelivered
notice on terminal result delivery. This replaces the earlier choice to preserve
notices queued before a result wait. A notice already taken at the core's delivery
boundary stays; a parent that never reads the result still gets its notice.

## Decision

1. **Shell diagnostics.** After any recognized filter applies, retain the first
   80 original lines matching `error`, `warning`, `panic`, or `failures`
   (case-insensitive), or literal `FAILED`. Keep each line verbatim, including
   repeated occurrences; recover lines a filter removed without duplicating those
   it retained. Keep the filter's other failure detail and ordering where possible.
   The existing head/tail byte limit remains the final backstop.
2. **Raw hint.** End the summary body with `raw: true for the full N lines`, where
   N counts the captured input lines, including trailing blank lines before
   presentation trimming. The existing filter marker, output-recovery
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
   withdrawn if still pending when that wait delivers the terminal result. The
   worker receipt retains the inbox and notice identifier, including notices
   restored after an abandoned wait. Delivery withdraws only that turn's notice;
   another child or continuation's notice is unaffected.
   Cancellation is checked before acknowledging a terminal result, including a
   result that completed before an already-canceled observer was polled.
   **Generic inbox hook.** `Inbox::send_tracked(kind, text)` returns an inbox-local
   receipt (`u64`); `Inbox::withdraw(receipt)` removes only a still-queued entry.
   Normal `send` remains compatible. The receipt survives a journal-failure
   requeue, but is not journalled or model-visible. The delivery boundary atomically
   takes the queue; withdrawal never alters taken messages or history. The core
   has no worker, tool, or completion-specific logic; the worker service alone
   decides when to withdraw.
4. **Frozen expectations, owner-approved only.** The original approvals cover:
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
   The consolidated "82: A" approval additionally covers filter expectations and
   fixtures changed only by this behavior in:
   - `crates/p1-tool-shell/tests/output_filters.rs`
   - `crates/p1-tool-shell/tests/declarative_filters.rs`
   - `crates/p1-tool-shell/tests/pytest_filters.rs`
   - the tests in `crates/p1-tool-shell/guest/src/lib.rs`
   - `crates/p1-module-tests/tests/shell_boundary.rs`
   - `crates/p1-module-tests/tests/shell_stored_output.rs`
   The repair changes the cargo compile-error and failing-vitest samples to
   expect raw fallback in both native output-filter and component parity tests:
   retaining failure detail plus the hint and marker is larger than these logs.
   No fixture is lengthened in this repair. Other expectations in the six files,
   byte-for-byte parity and corpus reduction bars remain unchanged.
   The "82 stall A" approval additionally changes only
   `crates/p1-host/tests/stall_guard.rs::delegated::a_child_that_summarizes_without_progress_fails_with_the_parents_message`
   to require absence of `Worker w1 finished (failed)` after a waiting result
   already delivered the failure, checking history after the `c2` tool result in
   every parent request. A notice earlier in history remains legitimate.
   The existing failure-message and turn-count
   assertions remain unchanged. Nothing else in that file or otherwise frozen changes.

## Consequences

- The model sees diagnostics without rerunning a command and gets a concrete raw
  escape hatch. Large diagnostic sets remain bounded.
- A pending worker-result wait and the inbox do not deliver the same completion
  twice, including completion before the wait begins; abandoning a wait does not
  lose the completion notice. Notices already delivered to history remain there.
- Existing corpus reduction, raw passthrough, capture-loss and byte-bound
  guarantees still apply. Timings remain available in raw output.

## Alternatives considered

- **Change frozen tests without approval.** Rejected per ADR-0126; the owner
  approved the original three, the named declarative expectations/fixtures, and
  behavior-only filter changes in the six files listed in "82: A", then the one
  host stall-guard duplicate-notice assertion in "82 stall A".
- **Suppress every notice whenever any wait starts.** Rejected: canceled or
  dropped waits have not delivered a result, and lifecycle waits still need notices.
- **Weaken reduction bars to fit the hint.** Rejected: shorten successful test
  summaries instead, preserving counts and diagnostic detail.

## Evidence

Issue #82 and its linked approval and conflict-list comments above. Frozen suites named in the decision;
`p1-shell-guest` exercises diagnostic recovery across all five match classes and
the 80-line boundary; `p1-workers` exercises canceled and dropped waits, concurrent
waiters and continuation notices through the public service interface.
