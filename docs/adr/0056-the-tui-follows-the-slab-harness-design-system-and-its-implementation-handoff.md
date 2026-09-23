---
adr: 56
title: The TUI follows the SLAB Harness design system and its implementation handoff
status: proposed
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/design/tui/slab/TUI-HANDOFF.md, docs/design/tui/slab-harness/readme.md]
---
# ADR-0056: The TUI follows the SLAB Harness design system and its implementation handoff

## Context

The owner designed a new visual language for p1's terminal UI in Claude Design, the SLAB Harness
design system, and said of it: "This is our new TUI design." Its readme declares itself
authoritative over `docs/design/tui/SPEC.md` wherever they conflict, most visibly on SPEC's
monochrome rule. SLAB Harness covered the transcript grammar only. The owner then asked for the
missing parts to be designed "specifically for what we actually have, need and will have" and
handed to the implementer. A Claude Design session (Opus 5.5, high) read `main@5b9c49a` and
produced `docs/design/tui/slab/TUI-HANDOFF.md`: geometry, the row rule, every transcript element,
per-tool Blocks, approvals, workers, the pane, the statusline, the event→element map, keys and
colour fallbacks, with 25 element mocks and 34 full screens as exact cells (`grids.json`).

## Decision

`p1-tui` renders what `docs/design/tui/slab/TUI-HANDOFF.md` specifies, on the SLAB Harness
tokens in `docs/design/tui/slab-harness/`. Where the handoff and `SPEC.md` disagree, the handoff
wins (its §13 lists every departure). `SPEC.md` stays valid for what the handoff does not cover.
The mocks in `grids.json` are the snapshot oracle: a renderer change is done when the matching
mock's TEXT and RUNS equal the rendered buffer.

## Consequences

- Signal hues on glyphs and outcome markers, three-band tool Blocks, an inline Decision band, a
  statusline at every width, and pane widths 38/56/split replace SPEC's monochrome single-line
  grammar. Existing snapshot tests that encode the old look are rewritten as the new renderers
  land.
- Tool names stop being matched in `p1-tui`: one `tool_face` adapter lives in `p1-host` until
  #46 lets tools describe their own call.
- §14 of the handoff lists needs that are not planned (tool name in `ToolInputDelta`, a diff
  seam, a session index for `/resume`, and others). They are rendered as absent or `—` until
  they exist, never faked.
- §15's open questions stay open for the owner. The implementation takes the handoff's
  recommendation for each until the owner answers.

## Alternatives considered

- Keep SPEC.md and restyle it by hand: rejected, since the owner declared SLAB Harness the new design.
- Implement from the SLAB Harness components alone and fill the gaps ad hoc: rejected. That
  would leave responsive geometry, errors, workers and approvals to the implementer's taste.

## Evidence

- `docs/design/tui/slab/grids.json` parses to 25 element and 34 screen mocks. Every RUNS row
  covers exactly its width (`cargo test -p p1-tui --test slab_grids`).
- The design session's brief is `docs/design/tui/slab/BRIEF.md`; its provenance is in
  `docs/design/tui/slab/README.md`.
