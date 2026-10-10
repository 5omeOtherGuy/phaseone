---
adr: 171
title: Interactive sessions are journaled by default in one state-root session catalog
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0171: Interactive sessions are journaled by default in one state-root session catalog

## Context

Issue #62 (owner-dispatched 2026-10-11) asks that every interactive p1 session be journaled without a flag, listed, and resumable by id. Without `--session` the journal was memory-only. Two state-directory resolutions existed: ACP's journal archive (ADR-0158, which recorded an ASSUMPTION pending #62) and the workflow run root (ADR-0053 item 7). The issue proposed `sessions/<workspace-slug>/<id>.jsonl`; the dispatch amended it to one state root and one catalog for the CLI and ACP, without orphaning ACP journals already written. The TUI is frozen, so it gets no picker.

## Decision

1. **State root.** `p1_host::state::root` is the one resolution: `$P1_STATE_DIR`, else `$XDG_STATE_HOME/p1`, else `$HOME/.local/state/p1` (blank means unset), read through the host's injected `Locations`. The session catalog, `p1 acp` and workflow runs without a session file (`<root>/workflows`) use it. This settles ADR-0158's assumption.
2. **Layout.** One catalog, `p1_host::session::catalog`: `<root>/sessions/<front end>/<id>/` holding `workspace.json` (the canonical workspace) and `journal.jsonl`. Front ends are `cli` and `acp`. The journal's existence publishes the session. Its worker journals, output store, scratch and workflow runs stay `journal.jsonl.*` siblings, so a session is one directory. This is ADR-0158's layout, generalized, so existing `sessions/acp/<id>` journals stay where they are. New ids for both front ends are `<yyyymmdd-hhmmss>-<6 hex>` (UTC); older `p1-…` ACP ids stay valid. ACP's `SessionArchive` adapter is the catalog scoped to `acp`.
3. **Default journal.** An interactive run (no prompt) whose stdin is a terminal, given neither `--session` nor `--resume-id`, journals to a new `cli` session. A headless run, or one with a non-terminal stdin, stays memory-only: workers, fanout and scripts name their files. If the catalog cannot be created, the run continues unsaved and says so. The session's id is the un-namespaced native option `session_id` of the parent's assembled options, so every `Environment` record names its session. No adapter consumes a key outside its namespace (ADR-0039), and p1-contracts and p1-core are unchanged. At exit the host prints the id and the resume command.
4. **CLI surface.** `p1 --resume-id ID` continues catalog session ID, from either front end. An interactive `--resume` without `--session` continues the newest session of the workspace. A headless one still needs `--session`. A held writer lock fails the run, naming the journal file. `p1 sessions [--workspace DIR] [--json]` lists every session newest first: id, front end, start time, workspace, last route and model, first prompt (80 characters at most), record count, whether a live process holds the lock, and a warning when the journal is torn or unreadable (listed, never skipped). `p1 sessions prune --older-than DAYS [--workspace DIR]` prints what it would delete. `--yes` deletes each idle, non-live session directory under its writer lock. Nothing is deleted automatically.

## Consequences

An operator who never passes a flag has listable, resumable history, and a front end has a data source for a resume picker. `--session FILE` and `--session FILE --resume` are unchanged. Lock and resume semantics stay ADR-0031's. `P1_STATE_DIR` now also moves workflow run directories. Start time is the first record's `at_ms`, else when `workspace.json` was written. Prune age is the journal's last write. Listing reads each journal in full (256 MiB bound), so a very large catalog lists slowly. Live status is a non-blocking shared-lock probe: a process starting in the same instant may see the probe's lock. `--resume-id` on an ACP session written before this record adds one `Environment` record, because the stamped id changes the options.

## Alternatives considered

The issue's `sessions/<workspace-slug>/<id>.jsonl` would mix worker journals, outputs and scratch of several sessions in one directory, need a slug rule, and orphan or migrate ACP's directories. A `session_id` field on `ModelOptions` would change the provider wire schema and the frozen core acceptance tests that build the struct. A field on the assembly identity line is `deny_unknown_fields` and would need a journal version bump. A separate index file would be a second truth beside the journals. Automatic retention was out of scope by the issue.

## Evidence

`cargo test -p p1-host --test session_history`: the interactive default creates one 0600 journal whose `Environment` names its id. A headless or piped run writes nothing under the state dir. `p1 sessions --json` orders three CLI and ACP sessions by start and lists a torn journal with a warning. `--resume-id` replays a free session and names the file of a locked one. `--resume` picks the workspace's newest session. Prune is dry until `--yes` and never takes a live session. `cargo test -p p1-host --test session_archive` covers ACP on the shared catalog code. Live TUI check: NOT RUN here (lead).
