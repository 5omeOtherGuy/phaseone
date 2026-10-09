# p1-tui — frozen

Frozen by the owner on 2026-10-09. This crate and its driver, `crates/p1-host/src/tui.rs`,
are a parts donor, not p1's TUI.

- Do not extend, improve, redesign or plan work on it, and do not base new front-end work on it.
- Copy useful pieces (renderer, snapshot tests, key mapping) into new code and name the
  source path in the commit message.
- When another change breaks its build or tests, restore them with the smallest mechanical
  edit that changes no behaviour; nothing else.
- The owner decides p1's new front end; raise questions in `~/.agents/xo/for-owner.md`.

Background: ADR-0043 (pure state machine plus host driver) and ADR-0056 (SLAB design).
