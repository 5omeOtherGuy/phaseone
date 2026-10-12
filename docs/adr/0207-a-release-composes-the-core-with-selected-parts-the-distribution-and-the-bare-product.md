---
adr: 207
title: A release composes the core with selected parts, the distribution and the bare product
status: superseded
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: [217]
sources: [https://github.com/5omeOtherGuy/phaseone/issues/855, https://github.com/5omeOtherGuy/phaseone/issues/840, docs/adr/0200-phaseone-s-core-is-the-bare-product-subscription-connectors-and-the-owner-pack-are-optional-parts.md, docs/adr/0196-the-credential-trust-anchor-is-compiled-from-the-build-s-catalog-roots-and-a-release-packs-the-same-roots.md, docs/adr/0065-p1-installs-from-its-release-channel-into-a-prefix-and-the-binary-names-its-commit.md]
---
# ADR-0207: A release composes the core with selected parts, the distribution and the bare product

## Context

The repository split (epic #840, ADR-0200) made the core the bare product and the subscription
connectors (`connectors/subscriptions/`) and the owner pack (`packs/owner/`) optional parts.
Until now every release published one asset set: the default build with the connectors, and
since #853 the owner pack too, so the owner's installed p1 kept working. The bare product had a
gate job (`bare`) but no release. The owner's plan for the split (#855): `stage-release.sh
--parts subscriptions,owner`, `install.sh` accepts the composed share, and `release.yml` builds
the distribution asset for the owner's install and the bare asset.

## Decision

`scripts/stage-release.sh --parts <part,...>` composes a share from the core catalog (`.`) and
the named parts, `subscriptions` (`connectors/subscriptions`) and `owner` (`packs/owner`); an
empty list is the core alone, and `--asset-name` names the four assets. Every release of a main
commit publishes two compositions, built and staged by `release.yml`:

- the **distribution**, under the existing names `p1-linux-x86_64` and `p1-share.tar.gz`: the
  default build anchoring `.:packs/owner` plus the connectors, staged with
  `--parts subscriptions,owner`;
- the **bare product**, under `p1-bare-linux-x86_64` and `p1-bare-share.tar.gz`: the build
  without the `subscriptions` feature anchoring `.`, staged with `--parts ""`.

`scripts/install.sh` installs the distribution by default and the bare product with `--bare`;
it records the set it installed (`share/p1/.p1-assets`), reinstalls when the other set is asked
for, and writes an updater that keeps the set.

## Consequences

- The owner's existing installs and their updaters, which download the fixed names, keep
  receiving the distribution: its asset names did not change.
- Anyone can install the bare product from the same release, whose binary carries no connector
  code and whose share has the core catalog only (`default`, the API-key routes, no accounts).
- The release job builds p1 twice (release profile, both feature sets); its timeout grows from
  45 to 60 minutes.
- Both compositions ship the same module packages, built once with their default features, so
  the bare share's provider components still contain the subscription dialects. The bare host
  refuses connector route and credential names before any module is configured (#851), so that
  code is never reached; building the modules without the feature is a later step if wanted.
- Each binary must anchor exactly the roots its share packs (ADR-0196). `--parts` replaces the
  list of `--catalog-root` flags in the release workflow, the gate's release candidate and the
  installed-release test; `--catalog-root` remains for other roots.

## Alternatives considered

- One release per composition (separate tags): doubles the tags, and the updater would need to
  learn which tag series to follow.
- A share that records its parts in `modules/manifest.json`: the release manifest's top-level
  keys are a fixed set checked by the installer and the binary; a marker file beside the
  installed share (`.p1-assets`) carries what the installer needs.
- Building the bare product's modules without the `subscriptions` feature: more build time for
  code the bare host never reaches; left for later.

## Evidence

`python3 scripts/test_stage_release.py` (every composition ships exactly the core and its parts;
unknown parts and two root lists are usage errors; the asset name names all four assets),
`python3 scripts/test_install.py` (`--bare` downloads the bare set, its updater stays bare,
switching sets reinstalls the same release, an install from before the marker is the
distribution, `--bare` with `--local` is refused), `test_release_manifest.py` (each build's
anchor equals its staged parts), `test_release_publication.py` (eight assets), and the gate's
`release candidate` step. A bare debug binary staged with `--parts "" --asset-name p1-bare` and
installed with `install.sh --bare` from a `file://` release: three core routes, no accounts,
`p1 env check` 1 ok, `p1 env show default` resolved, updater `--latest --bare`.
