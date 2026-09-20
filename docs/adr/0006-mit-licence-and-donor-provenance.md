---
adr: 6
title: MIT licence and donor provenance
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [D3, LICENSE, AGENTS.md, docs/SLICE-REPORT.md]
---
# ADR-0006: MIT licence and donor provenance

## Context

D3 (lead): p1 is licensed MIT, copied from iris-agent's `LICENSE` (same owner), after a
licence check of the donor. The donor's `NOTICE` lists Codex-derived Apache-2.0 files only
under `src/ui/tui/streaming/*`, which p1 does not take.

## Decision

p1 ships under MIT. Any donor file carrying an SPDX Apache header keeps it and gets a
`NOTICE` entry. Copied code is named by donor path in the commit message (`AGENTS.md`,
"Hard rules").

## Consequences

The licence is unambiguous and compatible with the donor. Provenance stays traceable
because each copied file points at its donor. No extra `NOTICE` entry is needed for the
current slice.

## Alternatives considered

None recorded.

## Evidence

`LICENSE` is the MIT text, copyright 2026 5omeOtherGuy. D3 cites iris-agent@62c8345
`NOTICE`. `docs/SLICE-REPORT.md` section "What came from Iris" names the copied files.
