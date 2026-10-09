---
name: slab-harness-design
description: Use this skill to generate well-branded interfaces and assets for SLAB Harness (the Phaseone coding-agent terminal UI — three-band BLOCK transcript, decisions, delegation, composer, statusline), either for production or throwaway prototypes/mocks/etc. Contains essential design guidelines, colors, type, fonts, assets, and UI kit components for prototyping.
user-invocable: true
---

> **Frozen (owner 2026-10-09).** `p1-tui`, `crates/p1-host/src/tui.rs` and everything under
> `docs/design/tui/` are a parts donor, not p1's TUI. This document is no longer a build contract,
> plan or task: do not implement, extend or plan work from it. Read it only to copy a useful
> piece into new code, naming the source. The owner decides p1's new front end.

Read the README.md file within this skill, and explore the other available files. This design system is authoritative: where it conflicts with SPEC.md, BLOCK-SPEC.md or older preview files (including their monochrome rule), follow this system.
If creating visual artifacts (slides, mocks, throwaway prototypes, etc), copy assets out and create static HTML files for the user to view. If working on production code, you can copy assets and read the rules here to become an expert in designing with this brand.
If the user invokes this skill without any other guidance, ask them what they want to build or design, ask some questions, and act as an expert designer who outputs HTML artifacts _or_ production code, depending on the need.
