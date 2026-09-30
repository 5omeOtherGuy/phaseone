---
adr: 106
title: Edit falls back to a whitespace-tolerant match and shows the closest region
status: accepted
date: 2026-09-30
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/tools.md, docs/adr/0001-p1-is-a-new-project-iris-is-a-parts-donor.md, docs/adr/0025-workspace-confinement-and-read-before-mutate.md]
---
# ADR-0106: Edit falls back to a whitespace-tolerant match and shows the closest region

## Context

ADR-0001 lists fuzzy edit matching and ADR-0025 records Iris's fuzzy edit matching as
deliberately left behind. The owner decided on 2026-09-30 to port that behaviour: an exact
`old_string` copied from a model transcript is regularly off by trailing whitespace or a
Unicode confusable, and the edit then fails with nothing to act on.

## Decision

`edit` tries the exact match first, unchanged. Only when it finds nothing does it match
again over text folded the way Iris folds it — Unicode spaces, curly quotes and Unicode
dashes become their ASCII form, trailing whitespace at the end of a line is dropped —
and apply a unique folded match, echoing the line-numbered region it applied. A match that
is not unique names the count, and a not-found error keeps p1's first sentence and appends
the closest region, with line numbers and Iris's bounds. Read-before-mutate, the
changed-on-disk refusal, atomic write, line-ending and final-newline preservation, the
output bounds and the four-property schema are untouched.

## Consequences

Fewer edits fail for a reason the model cannot act on, and a not-found error now points at
the region to re-anchor from. A unique folded match can apply to a region the model did not
exactly name, so the echoed region is what shows it what changed; the echo appears on
success only, never on failure.

## Alternatives considered

Keeping exact matching only (rejected: the owner decided to port the fallback); scoring
folded candidates to decide how close a "closest region" is (rejected: Iris's rule is one
shared word, and a threshold of our own would change the not-found text for reasons no ADR
records).

## Evidence

`docs/design/tools.md` (the `edit` section), the donor
`/home/phaseonebig/projects/iris-agent/src/tools/edit.rs` (`locate_matches`, `select`,
`not_found_error`, `normalize_for_fuzzy`), and the tests in
`crates/p1-tool-edit/logic/src/lib.rs`: `an_exact_unique_match_wins_over_the_tolerant_one`,
`a_unique_tolerant_match_is_applied_and_echoes_its_region`,
`an_ambiguous_tolerant_match_names_the_count`, `not_found_shows_the_closest_region`,
`the_closest_region_is_bounded`, `replace_all_replaces_every_tolerant_occurrence`.
