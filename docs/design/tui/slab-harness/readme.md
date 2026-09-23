# SLAB Harness — design system

Product layer for the **Phaseone (p1) coding-agent TUI**, built on **SLAB Core** (tokens, glyphs,
cell primitives). This system restates only what the harness adds: the three-band BLOCK
transcript, decision rows, delegation, the composer, the statusline and the right pane.

> **Authoritative.** This design system is the source of truth for the harness's visual
> language. Where it conflicts with `p1-tui-spec/SPEC.md`, `p1-block-spec/BLOCK-SPEC.md`, the
> preview HTML, or the "fixed rules" in the source project's CLAUDE.md — most notably the
> monochrome / diff-only-hue rule — **this system wins**. Those documents remain valid for
> anything this system does not cover (keybindings, journal, access model, non-goals).

Colour mode: **SLAB/SIGNAL**. Core signal hues apply to glyphs and outcome markers; structure
and geometry follow BLOCK-SPEC unchanged.

## Sources
- `p1-block-spec/BLOCK-SPEC.md` (geometry, bands, per-tool rendering, acceptance checks)
- `p1-tui-spec/SPEC.md`, `Phaseone TUI Scrollback.dc.html` (source project)
- Repo `5omeOtherGuy/phaseone` — vocabulary only (tool names, routes, access model)
- SLAB Core design system (vendored tokens in `tokens/fonts|colors|type|space.css`)

## Index
- `styles.css` → `tokens/*.css` (`harness.css` = band ladder, roles, geometry)
- `guidelines/` — Band ladder, roles, geometry, column stops, rhythm, glyphs, inversion, FAINT test
- `components/cell/` — Span, Band
- `components/transcript/` — Block, Decision, Worker, Picker, OperatorInput, Prose, Working
- `components/shell/` — Composer, Statusline, Pane
- `ui_kits/harness-session/` — full 120-column interactive session
- `ds.js` (dev loader) · `SKILL.md`

---

## CONTENT FUNDAMENTALS
- Transcript copy is **facts, not narration**. Outcome strings: `✓ 212 lines · 6.1 kB`,
  `✗ 11.4s · exit 101`, `+1 −1 · 1 of 1 files`. Facts joined with ` · `.
- Tool names lowercase, verbatim: `read shell write edit skill search send stop delegate ask notify compact`.
- Operator input echoes verbatim after `› `. Assistant prose is plain sentences, no headings,
  no role label, no "Sure!", no emoji.
- Decision labels are short verbs/nouns: `allow once`, `session`, `project`, `deny`, `review`, `apply`, `discard worktree`.
- Ungrantable options state the reason inline: `not grantable — destructive floor`.
- Unknown cost/spend is `—`, never `0`. Fold handles are `[h-xxxx]` (4 hex).
- No mythology names in user-visible strings. Route names are product names (`Fable 5.1`), not codenames.
- Key hints use caret notation: `^O open in pane`, `^C cancel`, `⏎ confirm`, `esc dismiss`.

## VISUAL FOUNDATIONS
- **Surfaces:** exactly three — GROUND `#0a0a0a`, BLOCK `#121212`, BLOCK+ `#1c1c1c`. Only tool events,
  the composer, the pane and the statusline get a fill. Chrome is earned.
- **Bands:** header (BLOCK+, exactly 1 row) → body (BLOCK, 1..N) → meta or decision (0–1). Bands are
  flush; the step is the separation. Every band fills to the full width — trailing cells painted.
- **Geometry:** 120 cols = 2 inset · 76 transcript · 2 gutter · 38 pane · 2 inset. Band padding 2 → every
  text margin is column 3. Tool name field 10 cols. 1 blank GROUND row between events; never 2.
- **Ink:** INK arguments/values; DIM tool names/body/outcome facts; FAINT line numbers, fold meta,
  key hints, timestamps only.
- **Hue (Signal):** glyph and outcome marker only — ✓ green, ✗ red, ! amber, running ▸/▪ cyan, paths blue.
  Diff rows use the tint pair. Body text is never hued.
- **Inversion:** amber-inverted 3-cell decision keys and the focused picker row. Statusline route chip is
  neutral (FG) inverted. Nothing else inverts.
- **Borders / radii / shadows / gradients:** none. No box-drawing characters (U+2500–257F) ever.
- **Motion:** one — the working indicator (▪▪▪, 1.1s, 0.18s stagger). `P1_REDUCED_MOTION=1` freezes it.
  Settled rows never re-render. The transcript is append-only.
- **Folding:** >40 body rows → keep 24 (head for read/write/edit, tail for shell) + FAINT fold row.
- **Degradation:** NO_COLOR must still read from glyphs, ± column and layout alone.

## ICONOGRAPHY
Same eight glyphs as Core — `› ▸ ✓ ✗ ! · ↳ ▪` — with fixed hue partners (see `guidelines/glyphs.html`).
No icons, badges, per-tool colour coding, spinners (braille) or emoji. No logo; the product is named in type.

## Intentional additions
- `Band` — generic filled fixed-width row with right-aligned segments and left truncation; not a
  named element in BLOCK-SPEC, but every named element is composed from it.
- `Pane` — BLOCK-SPEC marks the right pane as a concept, not a contract.
