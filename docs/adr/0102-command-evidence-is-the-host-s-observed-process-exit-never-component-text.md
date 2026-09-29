---
adr: 102
title: Command evidence is the host's observed process exit, never component text
status: proposed
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0102: Command evidence is the host's observed process exit, never component text

## Context

The completion gate lets a `shell` run count as evidence that a check passed. The exit
status used to come from the model-visible `[exit code: <n>]` footer, which the
command's own output can print: a component or a compromised guest could forge a
successful run. The F3 work (#439, #444, #460) moved command evidence to the host's
`ProcessService` record and, to keep that evidence honest across a restart, made the
durable `ToolFinished` journal record carry the exit the host observed.

## Decision

The only source of command evidence is the exit status the host's `ProcessService`
observed for the call; model-visible text never sets it. The `Tool` trait exposes it
through `command_exit_code` (a non-consuming read so the host can journal it and hand
it to the session log) and `take_command_exit_code` (the live tee's consuming read);
`synthetic_command_result` marks an in-memory test double that may still supply a
footer. `RecordBody::ToolFinished.exit_code` stores the observation as an
`Option<Option<i32>>`, so an ABSENT field (a journal written before the field
existed, when the footer was the evidence format) is distinct from an explicit
`null` (this host observed no exit, so the footer has no authority) and from
`Some(code)`.

## Consequences

A current run cannot be made to pass by text alone, and a resume replays the host's
own observation instead of guessing from the footer. A legacy journal still verifies
its past runs through the footer, so history is not silently invalidated; a null
observed exit is never backfilled from the footer. Every `Tool` wrapper must forward
the two new methods or evidence is lost through it. The field's shape is a
compatibility commitment: absent, null and a value must keep their three meanings.

## Alternatives considered

Keep deriving evidence from the footer (rejected: forgeable). Drop the footer from
legacy journals entirely (rejected: breaks runs recorded before the field). Keep the
observation only in memory (rejected: resume would lose the evidence the gate needs).
Use a plain `Option<i32>` (rejected: it cannot tell an absent legacy field from an
explicit null).

## Evidence

Host tests in `crates/p1-host/src/activity.rs`:
`forged_component_footer_is_not_evidence_live_or_replayed`,
`replayed_host_exit_is_evidence_for_a_module_tool`,
`a_legacy_journal_still_counts_its_shell_exit`,
`host_observed_exit_overrides_spoofed_footer`. Journal round-trip in
`crates/p1-journal/tests/impl_journal.rs`:
`jsonl_keeps_absent_null_and_observed_exit_apart`. Report classification in
`scripts/test_run_report.py`: `test_host_observed_exit_overrides_a_forged_footer`,
`test_a_null_host_exit_is_not_taken_from_the_footer`,
`test_a_legacy_journal_still_reads_the_footer`. Design:
`docs/design/completion.md`.
