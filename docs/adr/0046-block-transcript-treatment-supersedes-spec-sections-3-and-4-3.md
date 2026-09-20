---
adr: 46
title: BLOCK transcript treatment supersedes SPEC sections 3 and 4.3
status: proposed
date: 2026-09-20
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0046: BLOCK transcript treatment supersedes SPEC sections 3 and 4.3

## Context

The owner supplied a revised transcript treatment, `BLOCK-SPEC.md` (delivered as
`p1-block-spec/`, with a ten-slide `preview.html` visual reference), and asked for it to
be implemented in `p1-tui`. The document states that it supersedes §3 (the column grid /
one-line tool row) and §4.3 (tool call + fold) of `docs/design/tui/SPEC.md`; the rest of
that spec — palette, the eight glyphs, the right pane, the 80-column floor, keybindings,
non-goals — still applies. A tool event now earns a three-band block (header on BLOCK+,
body on BLOCK, meta on BLOCK) instead of a single line.

## Decision

The transcript renders every tool event as the three-band BLOCK block from
`BLOCK-SPEC.md`, and that document supersedes `SPEC.md` §3 and §4.3. Folding is 40 rows
(keep 24; head, except `shell` which keeps its tail). No box-drawing codepoints are
emitted; bands are told apart by background fill alone. Nothing a reader must read is
FAINT. The in-repo contract is `docs/design/tui/BLOCK-SPEC.md`.

## Consequences

- Tool rows are taller: a settled call is header + body + meta, and a folded body is 26
  rows, so at the 80×24 floor a folded block is taller than the viewport and the header
  is reachable by scrolling. Below the 40-row threshold nothing folds.
- The per-tool outcome facts are derived from the tool's own output and input (read line
  and byte counts, shell exit code and elapsed, edit `+A −B` from old/new strings, search
  hit count). Facts the current events do not carry — `write` new-vs-replaced, edit file
  line numbers, delegate worker groups, `ask`/`notify`/`compact` payloads — use the
  generic shape or are omitted; the spec's own README marks those as a second pass.
- A blocking approval still owns the screen through the existing `render/diff.rs` and
  `render/permission.rs` decision rows rather than as band C of the block, because
  approvals are a separate state-machine overlay (ADR-0043).
- `NO_COLOR` is not wired to a colour-strip flag; colour-stripped legibility is enforced
  by tests instead (the substance of the §6 acceptance check).

## Alternatives considered

- Keep the one-line call row and only add fold handles (rejected: the owner's spec is
  explicit about the three bands).
- Fold a settled successful call away entirely (rejected: BLOCK-SPEC §11 says expanded is
  the default; nothing folds below 40 rows).

## Evidence

- `crates/p1-tui/src/render/block.rs` — the three-band renderer and its tests
  (band-A-one-row, width sweep 80/100/120/200, no box-drawing, bounded folds, golden
  frames per tool, FAINT discipline).
- `crates/p1-tui/src/fold.rs` — 40/24 head/tail fold rules.
- `crates/p1-tui/src/transcript.rs` — body source, fold registration, edit diff handle.
- `crates/p1-tui/tests/snapshots.rs`, `crates/p1-tui/tests/m2_stream.rs` — full-screen
  frames at 120×40 and 80×24.
- `scripts/gate.sh` green on `task/block-spec`.
