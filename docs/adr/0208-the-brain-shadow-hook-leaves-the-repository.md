---
adr: 208
title: The brain shadow hook leaves the repository
status: accepted
date: 2026-10-12
deciders: owner
supersedes: [58]
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/885, https://github.com/5omeOtherGuy/phaseone/issues/840, https://github.com/5omeOtherGuy/phaseone/issues/711, docs/adr/0200-phaseone-s-core-is-the-bare-product-subscription-connectors-and-the-owner-pack-are-optional-parts.md]
---
# ADR-0208: The brain shadow hook leaves the repository

## Context

ADR-0058 added `p1-hook-shadow`, a host-composed, detached and fail-open observer that handed
every committed user input and initial worker brief to the owner's `brain-packet-shadow`
binary, behind the default-on p1-host feature `shadow-hook` and the `settings.toml` table
`[shadow] brain_packet_shadow`. The repository split (epic #840, ADR-0200) makes phaseone the
bare product and everything built around it optional parts. Owner, 2026-10-11 (#885): the
brain shadow hook is the owner's personal use case and has nothing to do in this repository;
remove it instead of parking it behind the hook port.

## Decision

The crate `p1-hook-shadow`, the p1-host feature `shadow-hook`, its wiring around the parent
and worker journals, the `[shadow]` settings table and the hook's tests are removed. p1 ships
no hook. The hook seam stays the planned port of #711 (a `HookRunner` around the assembled
policies); an observer like the shadow hook returns, if at all, as a module or an external
part on that port, not as host code.

## Consequences

- The default build no longer spawns `brain-packet-shadow`, whether configured or found on
  `PATH`, and no longer writes its task files. The owner's adaptive-memory programme loses its
  feed from p1 until it plugs into #711 or another external interface.
- `settings.toml` rejects unknown keys, so a settings file that still holds a `[shadow]` table
  now fails to load with an error naming the key; delete the table before installing a
  release that contains this change.
- p1-host's feature list, the workspace and the shipping graph shrink by one crate; the bare
  build's feature set is `delegation,workflows` with no hook to mention.
- The hooks row of `docs/design/seams.md` records that no hook is in tree.

## Alternatives considered

- Keep the crate behind its feature, off by default: rejected by the owner; the code would
  stay in the product repository for one user's setup.
- Move it to the owner pack (`packs/owner`): a pack holds data and process scripts, not Rust
  linked into the host; it needs the hook port first.

## Evidence

`cargo build`, `cargo clippy --all-targets -- -D warnings` and the full tests of p1-host with
default features and with `--no-default-features --features delegation,workflows`; no file
names `p1-hook-shadow`, `p1_hook_shadow`, `shadow-hook` or `brain_packet_shadow` outside
history (`rg` over the tree, excluding `docs/adr` and the dated audits);
`scripts/check-module-boundaries.sh --shipping` without the crate.
