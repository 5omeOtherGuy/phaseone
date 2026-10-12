---
adr: 215
title: The subscription connectors live in their own repository, and the distribution pins one release by digest
status: superseded
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: [217]
sources: [ADR-0196, ADR-0200, ADR-0207, ADR-0209, ADR-0210, ADR-0211, ADR-0212, ADR-0214, issue #886, issue #893]
---
# ADR-0215: The subscription connectors live in their own repository, and the distribution pins one release by digest

## Context

Owner, 2026-10-11 and 2026-10-12, for wave 3 of the split (#886): "I want modules developed
outside this repository. The whole idea is that everybody can develop modules", and #893 as
its last step: the subscription connectors move to a repository of their own. #889 left them
self-contained under `connectors/` (ADR-0214): their credential-method and usage-probe modules
built with the guest SDK (ADR-0210, ADR-0212), their test crate and their catalog data (routes,
profiles). phaseone's build, gate and distribution (ADR-0207) still built them from that
directory.

## Decision

- **Boundary.** `git subtree split --prefix=connectors` moved the tree, history kept, into the
  public repository `5omeOtherGuy/phaseone-connectors`; phaseone keeps no copy. The contract
  between the repositories is the module interface: the WIT worlds and the value protocol
  (ADR-0210), through which the credential-method and usage-probe ports reach modules. The
  connectors name phaseone only through `p1-module-sdk` and, for their native suites, p1's host
  crates, all pinned at one phaseone revision. A revision, not the tag `module-sdk-v1.2.0`:
  at that tag phaseone's own `connectors/` workspace named the SDK by path, which made cargo
  skip the bindings crates of a git dependency (#893); the first revision that works is the
  one that named it by git with a `[patch]` back to the checkout.
- **A connector release** is a tag `v<version>` of that repository whose CI builds every
  package with phaseone's own build steps (`release.sh`: `build-modules.sh --external` and
  `pack-module.py` from the pinned checkout), runs p1's author conformance on each archive
  (`conformance.sh`) and publishes one `.p1pkg` per module (ADR-0209),
  `connectors-catalog.tar.gz` (`routes/` and `profiles/`, data only) and `SHA256SUMS`.
- **Version coupling.** phaseone pins one connector release in `connectors.lock`: the
  repository, the tag and the SHA-256 of every asset. `scripts/fetch-connectors.py --pin
  <tag>` writes the lock from that release's sums once; every fetch is checked against it. A
  fetch refuses an asset that differs, a catalog member outside `routes/` and `profiles/`, and
  a package archive that is not the frozen format, names a package in `p1/` or whose component
  differs from its manifest. A connector release built against WIT `x.y` loads in every p1 of
  major `x` and minor `>= y` (ADR-0210); a major change of the interface needs a connector
  release before phaseone moves its pin.
- **Composition.** `scripts/build-modules.sh --all` fetches the pinned release into
  `modules/target/connectors` (no download while the fetch's lock equals `connectors.lock`) and
  publishes each package beside p1's own build outputs, after the checks a built component
  takes (valid component, extracted WIT and imports, no WASI); `check-module-boundaries.sh`
  and the gate's module validation check them against their released manifest. A default
  build anchors the fetched catalog root when it is there (ADR-0196), and `stage-release.sh
  --parts subscriptions` ships it and the packages, refusing a fetch by another lock. The
  bare product never ships them.
- **Tests.** The connectors' unit and native suites (their logins through `p1-auth`, their
  probes through `p1-usage`) run in their repository. The distribution's pins of the routes
  it composes (`anthropic_route`, `openai_route`, `route_files`, `store_only`) moved back into
  `crates/p1-host/tests`, and read the fetched catalog.
- **Landing.** A connector change lands in the connectors repository alone. A user takes a new
  connector release with `p1 modules install <file>.p1pkg --confirm-origins` into any released
  p1. The distribution takes it when phaseone's pin moves: a pull request that changes
  `connectors.lock` only.

## Consequences

- phaseone holds no subscription code or data; its gate no longer builds or tests the
  connectors' crates, and the connectors' CI no longer waits on phaseone's.
- The distribution changes only through a reviewed lock bump, and a fetched asset can differ
  from what was reviewed in no byte.
- phaseone's build of the distribution and its route pins need the network once per lock (the
  fetch), as they already need it for crates; a checkout that has not run
  `scripts/build-modules.sh --all` builds and tests the core catalog alone.
- Two places to look for connector work.
- A WIT or protocol major change is a coordinated release: phaseone's interface first, then a
  connector release, then the pin.

## Alternatives considered

- **Keep `connectors/` in phaseone (ADR-0214's state).** Every connector change stays a
  phaseone pull request; contradicts #893's done-when.
- **Follow the connectors' latest release unpinned.** No review of what the distribution
  ships; a compromised release would reach every install. Rejected for the digest pin.
- **A git submodule.** Pins a commit, not built and checked artefacts; phaseone would compile
  the connectors again and need their build inputs. Rejected.
- **A new SDK tag for the working revision.** The SDK's version is the WIT version (ADR-0210)
  and the WIT did not change; a revision pin has the same reproducibility. Rejected.

## Evidence

- `scripts/test_fetch_connectors.py`: the pinned release unpacks into a catalog root, the
  package archives and the build layout; a changed asset, a catalog member outside
  `routes/`/`profiles/`, a `..` path, a symlink and a malformed package archive are refused; a
  fetch by the same lock downloads nothing; pinning rewrites the lock from the release's sums.
- `scripts/test_stage_release.py`: the packages ship only with the `subscriptions` part, and a
  fetch by another lock is refused.
- phaseone-connectors `v0.1.0`: all nine packages pass `conformance.sh`.
