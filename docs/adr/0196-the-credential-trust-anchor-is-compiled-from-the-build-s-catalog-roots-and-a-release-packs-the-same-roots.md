---
adr: 196
title: The credential trust anchor is compiled from the build's catalog roots and a release packs the same roots
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/850, https://github.com/5omeOtherGuy/phaseone/issues/840, https://github.com/5omeOtherGuy/phaseone/issues/766, docs/adr/0110-credential-sources-are-bound-to-endpoint-origins.md, docs/adr/0139-accounts-are-separate-from-routes.md]
---
# ADR-0196: The credential trust anchor is compiled from the build's catalog roots and a release packs the same roots

## Context

Owner direction 2026-10-11 splits the catalog into the core at the repository root,
subscription connectors under `connectors/subscriptions/`, and the owner's pack under
`packs/owner/` (epic #840, issue #850). The owner requires that "a release must pack
exactly the roots it anchored". ADR-0110 and ADR-0139 §2 compile the credential trust
anchor from source `routes/` and `accounts/`, never installation-prefix files; reading
only the repository-root directories would lose connector and owner origins after the
split. This record amends the source selection, not the runtime trust rules.

## Decision

`p1-host/build.rs` reads `P1_CATALOG_ROOTS`, a colon-separated list of roots relative
to the repository root or absolute. Unset means the single root `.`. Explicit roots
must be directories; absent `routes/` or `accounts/` contributes nothing. Empty root
entries are errors. Every root contributes its immediate `*.toml` files to the
compiled anchor, with the existing id, HTTPS and credential-method checks. Routes
resolve default accounts across all roots. Duplicate relative route or account file
paths are refused with both source paths. Cargo watches the variable and each root's
routes and accounts directories.

The host's default-on `subscriptions` feature reserves the #851 hook: when
`connectors/subscriptions/` exists, insert it after the first root unless already
explicitly selected. Its absence alone is tolerated. A core-only build after that
directory lands disables that feature; there is no installed-file trust fallback.

`stage-release.sh --catalog-root <path>` and `install.sh --local --catalog-root <path>`
are repeatable, default to `.` and resolve paths against the repository root. They
merge existing environments, routes, profiles and accounts directories into the
flat share layout. Duplicate relative files, including identical bytes, are refused
with both source paths rather than overwritten. `--local` sets the build variable
from its flags and also packs the optional feature-derived subscriptions root.
The installer's archive checks stay unchanged. The manifest generator reads no
catalog directories and needs no roots interface. Release builders pass the same
roots to compilation and staging; #855 adds distribution roots to the existing
release workflow's paired variable and flags.

## Consequences

A bare core-only build anchors only core origins. An owner's account on a
non-anchored origin needs `p1 login --trust-endpoint` unless its catalog root is
included in the build, subject to ADR-0110's unchanged borrowed-credential limits.
Distribution builds explicitly select core, connectors and the owner's pack.
The build's anchor does not depend on the installed data and is deterministic for
the same root contents. The default table remains unchanged before the split.

Release staging cannot infer a binary's selected roots: the builder is responsible
for matching `P1_CATALOG_ROOTS` plus feature-derived roots to staging flags. Local
installation derives both from one selection, ignoring a conflicting inherited
`P1_CATALOG_ROOTS`. Catalog roots containing `:` cannot be expressed in this interface.
Collisions fail instead of changing credential trust or silently replacing data.

## Alternatives considered

- Trust anchors from installed files: rejected by ADR-0110; a standalone binary
  must retain its trust anchor and installed files cannot approve themselves.
- A single merged source directory: obscures tier ownership and requires a separate
  preparation step before every build. Reading explicitly selected roots keeps the
  anchor tied to the source catalogs the release packs.

## Evidence

`cargo build -p p1-host` and comparison with the unmodified build script's generated
`shipped_routes.rs` check default-table compatibility. A scratch second catalog
build verifies a new route and account are anchored, and a duplicate route refuses
compilation. `cargo test -p p1-host --test credential_origins --test accounts --test
route_files` covers cross-root account lookup, missing per-kind directories,
duplicate-path refusal and unchanged runtime checks.

`python3 scripts/test_stage_release.py -q` and `python3 scripts/test_install.py -q`
exercise two roots and nested catalog files in the flat share, missing explicit
roots, duplicate refusal naming both sources, and local build/share root agreement.
`python3 scripts/adr.py index` and `python3 scripts/adr.py check` validate this record.
