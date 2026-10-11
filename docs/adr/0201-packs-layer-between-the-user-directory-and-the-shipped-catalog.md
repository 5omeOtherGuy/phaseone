---
adr: 201
title: Packs layer between the user directory and the shipped catalog
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: [845, 840, 766, 856, docs/adr/0139-accounts-are-separate-from-routes.md, docs/design/seams.md]
---
# ADR-0201: Packs layer between the user directory and the shipped catalog

## Context

The owner decided on 2026-10-11 (split epic #840, plan approved the same day) that phaseone's
core is the bare product and that the owner's own setup becomes an optional part: "even the
subscription connectores should not be part of the core product". The owner's catalog
(environments, routes, accounts, profiles, `modules.lock`, `subagents.toml`) moves to
`packs/owner/` and is installed outside the shipped share. The boundary itself is
ADR-0200 (#856); this record is the layering it needs (#845).

Until now the host searched two catalog layers, highest priority first: the user directory
`$P1_CONFIG_DIR/environments` (default `~/.config/p1/environments`) and the shipped one
(`$P1_ENVIRONMENTS_DIR`, else `<exe>/../share/p1/environments`, plus the source tree in debug
builds). Every consumer takes that ordered list, the first file per id wins whole, and finds a
layer's other directories as siblings, `<dir>/../routes` and so on (ADR-0139 §4). There was no
place for a third party's catalog except copying it into the user directory. Two gaps blocked
that: a layer without `environments/` hid all its siblings, because the OS cannot resolve
`<dir>/environments/..` when `environments/` does not exist; and `settings.toml` ignored
`P1_CONFIG_DIR`, so a run with its own config directory still read the home's settings.

## Decision

1. **A pack is a directory laid out like the catalog root**: `environments/`, `routes/`,
   `profiles/`, `accounts/`, `modules.lock`, with `subagents.toml` inside `environments/`.
   Any of them may be missing.
2. **Packs layer between the user directory and the shipped catalog.** The search order is
   the user directory, then `<pack>/environments` for each pack root, then the shipped
   directory (and, in debug builds, the source tree). Pack roots come from `P1_PACKS`, an OS
   path list taken as written, then from `packs = [...]` in `settings.toml`, each an
   absolute path or `~/…`; within each source in the order given. A relative `packs` entry
   is a settings error.
3. **A missing sibling is tolerated.** Every sibling lookup goes through one function,
   `p1_assembly::catalog_sibling`: an existing `environments/` keeps `<dir>/../<name>` (a
   symlinked `environments/` still finds the siblings next to its target), a missing one
   takes its parent as written. A missing sibling directory or file contributes nothing.
4. **`settings.toml` honours `P1_CONFIG_DIR`** first, then `$XDG_CONFIG_HOME/p1`, then
   `~/.config/p1`, so one variable moves both the user environments and the settings.
5. **No pack is added implicitly**, not even in debug builds: only `P1_PACKS` and `packs`
   add one.
6. The list is computed once per process (`p1_host::models::environment_dirs`). In-process
   workers copy it; `p1 acp` session processes inherit the environment and the settings file
   and compute the same list.

## Consequences

- The owner pack can live anywhere and be switched on per run (`P1_PACKS`) or per user
  (`packs`), without copying files into the user directory, and the bare product runs
  without it.
- `p1 env show` names the pack for every pack-provided environment, route, profile and
  account through its `source` lines, which print the file path under the pack root.
- A user directory without `environments/` now has its `routes/`, `profiles/` and
  `accounts/` seen too; before they were silently ignored.
- `settings.toml` is read once at startup to find the packs. A file that does not load adds
  no pack there; the commands that read settings report the error as before, and a command
  that needs no settings (`p1 modules verify`) still runs.
- A run with `P1_CONFIG_DIR` no longer reads the home's `settings.toml`. The p1 store
  (`auth.json`) still follows `XDG_CONFIG_HOME` and `HOME` only.
- The user environments still ignore `XDG_CONFIG_HOME` while settings follow it when
  `P1_CONFIG_DIR` is unset; this record leaves that difference as it was.

## Alternatives considered

- **A single user layer only**: install the owner's catalog into `~/.config/p1`. Rejected:
  it mixes the owner's pack with personal overrides, cannot be switched per run, and one
  file per id replaces the other with no record of where it came from.
- **Whole-directory replace** (`P1_ENVIRONMENTS_DIR` pointing at a merged tree): rejected,
  it drops the shipped catalog, so the pack would have to carry a copy of the core and
  follow every core change.

## Evidence

Tests added with this record, all on tempfile trees:

- `crates/p1-host/tests/models.rs`: `packs_layer_in_order_between_the_user_directory_and_the_shipped_catalog`,
  `p1_packs_come_before_the_packs_of_settings`,
  `p1_config_dir_moves_settings_as_it_moves_the_user_environments`.
- `crates/p1-host/tests/accounts.rs`: `packs_with_missing_siblings_layer_and_env_show_names_each_pack`
  (routes, accounts, profiles, `env show` and `p1 models` across four packs that each hold
  one directory).
- `crates/p1-host/src/catalog/subagents.rs`: `layers_without_a_configuration_are_passed_over`.
- `crates/p1-assembly/tests/environment_forms.rs`: `packs_with_missing_siblings_still_layer_their_profiles`;
  `tests/modules_lock.rs`: `a_pack_without_environments_or_without_a_lock_layers_what_it_has`;
  `tests/mcp_servers.rs`: `layers_without_the_environment_are_passed_over`.
