---
adr: 214
title: The subscription connectors live in their own repository, and the distribution pins one release by digest
status: proposed
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0196, ADR-0200, ADR-0207, ADR-0209, ADR-0210, ADR-0211, ADR-0212, issue #886, issue #893]
---
# ADR-0214: The subscription connectors live in their own repository, and the distribution pins one release by digest

## Context

Owner, 2026-10-11 and 2026-10-12, for wave 3 of the split (#886): "I want modules developed
outside this repository. The whole idea is that everybody can develop modules", and #893 as
its last step: the subscription connectors move to a repository of their own. #889 left them
self-contained under `connectors/`: their credential-method and dialect modules built with the
guest SDK (ADR-0210, ADR-0212), their test crate and their catalog data (routes, profiles).
phaseone's distribution build (ADR-0207) still composed them from that directory.

## Decision

- **Boundary.** `git subtree split --prefix=connectors` moves the tree, history kept, into the
  public repository `5omeOtherGuy/phaseone-connectors`. phaseone keeps no copy. The contract
  between the repositories is the module interface only: the WIT worlds and the value protocol
  (ADR-0210), through which the credential-method and dialect ports (ADR-0211, ADR-0212) reach
  modules. The connectors depend on phaseone only through `p1-module-sdk`, pinned by its
  `module-sdk-v<WIT version>` tag.
- **A connector release** is a tag `v<version>` whose CI builds every package with the pinned
  toolchain (`release.sh`, phaseone's own `build-modules.sh --external` and `pack-module.py`
  from the SDK checkout), runs p1's author conformance on each (`conformance.sh`) and
  publishes one `.p1pkg` per module, `connectors-catalog.tar.gz` (its `routes/` and
  `profiles/`, data only) and `SHA256SUMS` over all of them.
- **Version coupling.** phaseone pins one connector release in `connectors.lock`: the
  repository, the tag and the SHA-256 of every asset. `scripts/fetch-connectors.py --pin
  <tag>` writes the lock from that release's sums once; every fetch is checked against it and
  refuses an asset that differs and a catalog member outside `routes/` and `profiles/`. The
  distribution build (`release.yml`, `stage-release.sh --parts subscriptions`) fetches the
  pinned release, anchors its catalog root like any other (ADR-0196) and installs its packages
  into the release set (ADR-0209). The bare product never fetches it. A connector release
  built against WIT `x.y` loads in every p1 of major `x` and minor `>= y` (ADR-0210); a major
  change of the interface needs a connector release for it before phaseone moves its pin.
- **Landing.** A connector change lands in the connectors repository alone. A user takes a
  new connector release with `p1 modules install <file>.p1pkg` into any released p1. The
  distribution takes it when phaseone's pin moves: a pull request that changes
  `connectors.lock` only.

## Consequences

- The core repository holds no subscription code or data; its gate no longer builds or tests
  the connectors, and the connectors' CI no longer waits on phaseone's.
- The distribution changes only through a reviewed lock bump, and a fetched asset can differ
  from what was reviewed in no byte.
- Two places to look for connector work; the connectors' tests that need host crates run in
  their repository against the phaseone revision the SDK tag names.
- A WIT or protocol major change is a coordinated release: phaseone's interface first, then a
  connector release, then the pin.

## Alternatives considered

- **Keep `connectors/` in phaseone (status quo after #889).** Every connector change stays a
  phaseone pull request; contradicts #893's done-when.
- **Follow the connectors' latest release unpinned.** No review of what the distribution
  ships; a compromised release would reach every install. Rejected for the digest pin.
- **A git submodule.** Pins a commit, not built and checked artefacts; the distribution build
  would compile the connectors again and need their toolchain inputs. Rejected.

## Evidence

- `scripts/test_fetch_connectors.py`: the pinned release unpacks into a catalog root and the
  packages; a changed asset, a member outside `routes/`/`profiles/`, a `..` path and a symlink
  are refused; pinning rewrites the lock from the release's sums.
- A stand-in release (the SDK template's `acme/echo` plus the connectors' catalog) built by
  `connectors/release.sh` in 32 s, pinned and fetched with `fetch-connectors.py`, and its
  package installed with `p1 modules install` into release `main-9905d9454327`.
