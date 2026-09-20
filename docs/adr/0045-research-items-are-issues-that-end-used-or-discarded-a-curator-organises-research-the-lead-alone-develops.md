---
adr: 45
title: Research items are issues that end used or discarded; a curator organises research, the lead alone develops
status: accepted
date: 2026-09-20
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/design/research-program.md]
---
# ADR-0045: Research items are issues that end used or discarded; a curator organises research, the lead alone develops

## Context

p1 has cheap capable workers and a modular design, but one machine with 7 GB of RAM: Rust builds
cannot be multiplied. The owner asked (2026-09-20) for development to fan out, including research
on model-specific tools, prompting, compaction, transports, tool output and effort levels, under
two conditions: "all research / work needs to be evaluated and either discarded or used. Nothing
shall be done just for the sake of it and never touched again", and "Opus can handle
orchestrating reserach but NEVER development. That is your task. You are the lead on phase 1
development." A read-only consultation (issue #25) proposed the structure adopted here.

## Decision

A research question is a GitHub issue labelled `research` plus exactly one state label
(`research:queued|active|decision|implement|used|discarded`); it is admitted only with a named
consumer artefact and it ends `used` (merged and verified, or the baseline knowingly retained) or
`discarded` (reason and reopening condition recorded). Work in progress is capped — 3 active,
2 awaiting the lead's decision, 1 accepted awaiting implementation — and nothing new starts while
the decision queue is full. A short-lived research curator may dispatch read-only research leaves
and deliver decision memos; it never writes code, prompts, profiles, routes, ADRs or `STATUS.md`,
never dispatches implementation workers, never commits or merges, and makes no live provider call
without a budget the lead wrote down. Details: `docs/design/research-program.md`.

## Consequences

- GitHub is the only state authority for research; no second tracker. Open decisions are one
  `gh issue list --label research:decision` away, discarded ideas stay searchable.
- The lead's review capacity, not worker count, bounds the program; the caps make that explicit.
- Research leaves run no Rust build. Development slices that fall out of a memo are ordinary
  lead-owned briefs with reserved paths, and wait for a build lane like everything else.
- The arrangement is itself on trial: after two batches it stays only if it lowers lead effort
  per verified improvement.

## Alternatives considered

- A standing research supervisor that also dispatches implementation — rejected by the owner.
- Tracking research state in `STATUS.md` or the brain — a second authority that would drift.
- No structure, ad-hoc research jobs — exactly the "done and never touched again" the owner ruled out.

## Evidence

`../phaseone-briefs/fanout-program.answer.md` (consultation, 2026-09-20) and issue #25. The
dogfood record `split4a-deepseek` in `docs/dogfood/runs.jsonl` (855 requests, zero edits) shows
what unmeasured parallel work costs.
