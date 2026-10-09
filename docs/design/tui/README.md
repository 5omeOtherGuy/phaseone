# p1 TUI — design spec

> **Frozen (owner 2026-10-09).** This design and the `p1-tui` crate built from it are a
> parts donor, not p1's TUI. Do not implement, extend or plan work against anything in
> this directory. Read it only to copy a useful piece into new code, naming the source.

- `SPEC.md` — the implementable specification. Start here.
- `preview.html` — the rendered screens (needs `support.js` beside it). Visual
  reference only; `SPEC.md` is authoritative.

## History

This spec was the brief for the `p1-tui` crate (issue #12, ADR-0043, ADR-0056).
The spec is written to be read without the HTML.

## Scope

Terminal interface only. Nothing here touches the desktop environment.

## Provenance

Designed against `5omeOtherGuy/phaseone@main` — README, AGENTS.md, STATUS.md — for
vocabulary (tool names, routes, profiles, access model, journal, delegation). The design
itself is clean-sheet.
