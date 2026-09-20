---
adr: 30
title: Decisions are recorded as ADRs
status: accepted
date: 2026-09-20
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [DECISIONS.md, scripts/adr.py, docs/adr/README.md, AGENTS.md]
---
# ADR-0030: Decisions are recorded as ADRs

## Context

The first slice recorded decisions as rows of a flat ledger, `DECISIONS.md` (D1–D20), with
the reasoning behind the larger ones spread over `docs/design/` and `docs/SLICE-REPORT.md`.
A row has no room for alternatives or evidence, and a reversed decision (D12 → D20) was
visible only by reading both rows. After the slice the owner asked for ADRs and an ADR
system if none existed; none did.

## Decision

Decisions are Architecture Decision Records in `docs/adr/`, one file per decision, with fixed
front matter and sections, created with `scripts/adr.py new`. An accepted ADR is not edited
except for its status; it is reversed by a new ADR that supersedes it. `scripts/adr.py check`
and the script's unit tests run in `scripts/gate.sh`, so CI rejects broken numbering, a stale
index, one-sided supersede links, leftover template text and dead links. `DECISIONS.md` is
frozen as the historical ledger of the first slice; each row links the ADR it went into.
Small choices stay in commit messages (`AGENTS.md`, "Decisions").

## Consequences

Every decision of the first slice is an ADR (0001–0029), written by a worker from the
existing sources under the rule "write only what the sources support; otherwise 'None
recorded.'", then audited by the lead. `sources:` entries cannot be verified mechanically;
that stays a review task. `new` does not refresh the index: run `scripts/adr.py index`.

## Alternatives considered

Keeping the flat ledger; an external ADR tool (adr-tools, log4brains) — not installed on this
deliberately lean machine and not needed for a fixed, small format; the standard-library
script has no dependencies and runs unchanged in CI.

## Evidence

`scripts/adr.py check` exits 0 on 30 ADRs; `python3 scripts/test_adr.py` runs 24 tests, one
per `check` failure class. Lead audit on acceptance: no ADR cites a number absent from the
sources; the worker's handoff surfaced a stale statement in `docs/design/routes.md` §D
(reasoning replay "not yet proven live"), corrected in the same change.
