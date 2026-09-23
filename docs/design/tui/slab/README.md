# p1 TUI — SLAB Harness handoff

The build contract for the TUI redesign (ADR-0056). Where it and `../SPEC.md` disagree,
`TUI-HANDOFF.md` wins; every such place is listed in its §13.

| File | What |
|---|---|
| `TUI-HANDOFF.md` | The contract: tokens, geometry, row rule, every element, event map, keys, departures, and cell-exact mocks (TEXT + RUNS). Generated — do not edit. |
| `TUI-HANDOFF.src.md` | Its hand-written source (sections 1–15 without the grids). |
| `grids.json` | Every mock, machine-readable: `elements` (component level, at transcript/pane width) and `screens` (full terminal). The snapshot oracle in `crates/p1-tui/tests/common/slab.rs` reads it. |
| `lib/p1-cells.js` | Reference cell renderer (the Band rule and geometry as the designer implemented them). |
| `lib/p1-screens.js` | The state behind every mock — read it for a mock's exact input data. |
| `BRIEF.md` | The brief the design session worked from. |

`../slab-harness/` is the SLAB Harness design system itself (tokens, guidelines, components,
the 120×40 session kit), copied from the owner's export.

Provenance: designed 2026-09-23 in Claude Design (project "P1 TUI design handoff",
`claude.ai/design/p/b8aae144-483f-425a-aa29-480e95951367`, Opus 5.5 high) against
`main@5b9c49a`, on the SLAB Harness design system
(`claude.ai/design/p/cb3b9b3a-b5b2-4ae3-8f51-bdb1684185d6`). The JSX components of the
handoff project (24 new components, `_p1_bundle.js`) are not copied here; they stay in that
project until they are synced into the design system.
