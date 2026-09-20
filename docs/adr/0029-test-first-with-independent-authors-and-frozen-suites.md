---
adr: 29
title: Test-first with independent authors and frozen suites
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/SLICE-REPORT.md, D19, AGENTS.md, 4d440be, 708e1ab]
---
# ADR-0029: Test-first with independent authors and frozen suites

## Context

`docs/SLICE-REPORT.md` ("How the work was done") records the development method: a spec
with numbered steps, exact texts and an invariant list; independent test authors (sol and
glm-5.3) wrote the suites before implementation; deepseek implemented against the frozen
suites; the lead re-ran the gate, added adversarial cases and read the diff. `AGENTS.md`
("Hard rules") turns the frozen-suite rule into a permanent rule.

## Decision

Acceptance suites are written by authors independent of the implementer, frozen before
implementation, and never weakened, skipped or deleted. A held-out suite checks what the
frozen ones missed, and the lead adds adversarial and real-input tests. Red-first suites stay
on the task branch, never on `main` (ADR-0011).

## Consequences

The core passed 44/44 frozen tests on the first attempt and the held-out suite found no
defect. Every defect found in worker code in the slice was found by the lead's
adversarial/real-input tests or by live runs, not by the worker's own tests; the recurring
pattern was "implemented and unit-tested but not wired in". The method costs more lead time
and independent authors.

## Alternatives considered

Trusting a worker's own tests as acceptance. The results above are why the lead's
independent tests were kept; the slice report's defect list is the evidence that this
alternative failed in practice.

## Evidence

`docs/SLICE-REPORT.md` "How the work was done" (21 crates, 554 tests in the gate, 13
worker jobs, $4.58, about 2.6 worker-hours) and the acceptance commands. Commits 4d440be
(frozen core acceptance suites) and 708e1ab (lead acceptance tests).
