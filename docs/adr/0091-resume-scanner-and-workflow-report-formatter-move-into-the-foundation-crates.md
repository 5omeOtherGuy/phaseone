---
adr: 91
title: Resume scanner and workflow report formatter move into the foundation crates
status: proposed
date: 2026-09-27
deciders: lead
supersedes: []
superseded_by: []
sources: [issue #392, ~/.agents/xo/reports/cutover-fallbacks/PLAN.md rows p1-tool-delegate · S6 and p1-tool-workflow · S6]
---
# ADR-0091: Resume scanner and workflow report formatter move into the foundation crates

## Context

Since S6.11 the worker and workflow tools a main agent sees are member components; p1-host
registers no native `p1-tool-delegate` or `p1-tool-workflow` tool. It still linked both crates
for one pure function each: a resume read the workers an earlier process started with
`p1_tool_delegate::workers_started_in` (after rewriting every member identity into the native
one the scanner matched), and `p1 workflow run` built a native `WorkflowResultTool` only to
render the ended run's report. The S7.10 cutover removes native tool crates from the host's
normal dependency graph.

## Decision

The journal scanner moves into `p1-workers` as `p1_workers::journal::workers_started_in(records,
implementations)` with `STARTED_PREFIX`; the caller names the tool identities it reads, so the
host passes the native identity and the four worker member packages' and no longer rewrites
records. The report texts move into `p1-workflow` as `p1_workflow::render_report` and
`report_line`; `p1 workflow run` prints `render_report` of the report it waited for. Both native
tool crates reuse the moved functions (`p1_tool_delegate::workers_started_in` stays as a wrapper
over its own identity), and p1-host's `delegation` and `workflows` features no longer enable them.

## Consequences

`cargo tree -p p1-host -e normal` names neither `p1-tool-delegate` nor `p1-tool-workflow`, with
default or all features. `p1-workers` and `p1-workflow` each gain a small public function; the
report text stays one text, and the member component keeps its own copy of the rendering (a
guest cannot link the host crate), checked against the formatter by a parity test. A journal
written before S6.11 (native identity) and one written since (member identity) reserve their
worker ids the same way.

## Alternatives considered

Keeping the identity rewrite in the host and calling a moved scanner with one fixed identity:
it copies every record on resume for no gain. Having `run.rs` call the `p1/workflow-result`
component: the CLI report would then depend on a loaded package for a pure rendering.

## Evidence

`p1_workers::journal` tests; `p1_workflow::report` test pinned to the component's golden text;
`crates/p1-module-tests/tests/delegation_activation.rs`
`the_member_the_native_tool_and_the_formatter_render_one_report`;
`crates/p1-host/tests/resume_worker_ids.rs`
`a_resumed_session_journalled_under_the_native_identity_still_reserves_its_ids` (the pre-S6.11
identity, end to end);
`cargo tree --locked -p p1-host -e normal` (with and without `--all-features`).
