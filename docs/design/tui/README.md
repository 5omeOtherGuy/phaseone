# p1 TUI — design spec

- `SPEC.md` — the implementable specification. Start here.
- `preview.html` — the rendered screens (needs `support.js` beside it). Visual
  reference only; `SPEC.md` is authoritative.

## For Claude Code

```
Read SPEC.md. Implement the p1 TUI against it.
Sections 1-3 and 8 are fixed; section 5 is a concept to adapt.
```

The spec is written to be read without the HTML. The preview is there for when a
layout description is ambiguous and you want to see the real thing.

## Scope

Terminal interface only. Nothing here touches the desktop environment.

## Provenance

Designed against `5omeOtherGuy/phaseone@main` — README, AGENTS.md, STATUS.md — for
vocabulary (tool names, routes, profiles, access model, journal, delegation). The design
itself is clean-sheet.
