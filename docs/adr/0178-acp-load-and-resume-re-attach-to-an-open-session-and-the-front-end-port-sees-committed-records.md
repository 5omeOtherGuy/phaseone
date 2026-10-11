---
adr: 178
title: ACP load and resume re-attach to an open session, and the front-end port sees committed records
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: [774, 684]
---
# ADR-0178: ACP load and resume re-attach to an open session, and the front-end port sees committed records

## Context

The ACP TCK run on #684 (release main-a64ccbd21dbf) found p1 not conformant. `session/load` and `session/resume` of a session still open on the same connection answered `-32000 "session is already open"`, which fails ACP-LOAD-001/002/003 and ACP-RESUME-001/002. Separately, a turn's `session_info_update` followed the `session/prompt` answer, which fails the mandatory ACP-CANCEL-002 (#774). ADR-0156 keeps one session process per session and ADR-0158 keeps one journal writer, so a second process for an open session is not an option. That session process's replay source was the journal snapshot it resumed from (`FrontEndPort::history_loaded`), so a re-attached load could not show the turns that ran since.

## Decision

The router sends a load or resume of an open session to that session's own process, after the catalog has confirmed that the id belongs to that cwd. The process re-attaches: load replays the display transcript before the answer, resume replays nothing, and both answer the settings and publish the command list again. `FrontEndPort` gains the default hook `record_committed(&JournalRecord)`. The host calls it for every parent record its journal accepted, so the ACP front end's replay source stays the journal's records. The title, and a command list a reload changed, go before the prompt's answer, except for a client that opted out of the hold (#683), which keeps the answer-first order.

## Consequences

ADR-0156 and ADR-0158 hold: a re-attach starts no process and opens no writer, and the agent, settings, workers and workflow runs carry on. A cold load and a re-attached load replay through the same `replay::updates` over journal records. The hook is additive with a no-op default; the line front end and the frozen TUI ignore it. The ACP front end keeps the parent's records in memory for the life of its process, which is as large as the journal it already loaded. Inbox turns after a re-attach wait for the next prompt, as after any load (#788).

## Alternatives considered

Close the open process and launch a new one for the load: that cancels its prompt, workers and runs, so it is not a re-attach. Record the updates the driver forwarded as the replay source: that is a second transcript next to the journal, which ADR-0158 rejected, and it differs from a cold load's replay. Add a journal read to `SessionHandle`: it would mean plumbing the concrete store into the host's session handle, for the same records the commit path already passes.

## Evidence

`cargo test -p p1-acp`: `acp_load_and_resume_of_the_open_session_reattach` (driver), `acp_load_and_resume_of_an_open_session_reattach_to_its_process` (router, one process), `acp_nothing_follows_a_cancelled_prompt_answer`. `cargo test -p p1-host --test acp_fixtures`: `acp_fixture_session_load_replays` loads the open session again over the real host and replays the recorded turn plus the turn just committed. Fixtures re-recorded with `P1_ACP_RECORD=1`. TCK NOT RUN in this change (no credentials in orbs); the dispatcher re-runs it on the next release.
