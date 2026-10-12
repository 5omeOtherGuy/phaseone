---
adr: 216
title: Releases publish once a day from main's newest green commit and keep the newest 30
status: superseded
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: [218]
sources: [ADR-0065, ADR-0206, issue #921]
---
# ADR-0216: Releases publish once a day from main's newest green commit and keep the newest 30

## Context

ADR-0065 publishes a release after every green gate on `main` and prunes nothing. On 2026-10-12
that had produced 336 `main-*` releases in 18 days, about 19 a day, holding 10.8 GiB of assets;
272 of them had never had a binary downloaded. Only the latest release has a reader that needs
it (`p1-update` and the pack-only gate of ADR-0206); an older one serves only an explicit
`install.sh --from-release TAG`. The owner accepted the recommendation of a daily scheduled
release pruned to the last 30 (2026-10-12: "Your recommendation is accepted.").

## Decision

`release.yml` runs once a day (03:23 UTC) and on manual dispatch instead of after every gate. It
publishes the newest commit whose gate ran green for a push to `main`, unless that commit's
release already exists, and then deletes the `main-*` releases and their tags beyond the newest
30, oldest first and at most 100 a run. Everything else in ADR-0065 holds: the tag, the assets,
idempotent publication and the installer.

## Consequences

- At most one release build a day, and none on a day without a new green commit; the releases
  page holds about a month of history.
- The latest release lags `main` by up to a day. `p1-update` installs that day's commit, and the
  pack-only gate (ADR-0206) checks a pack against it rather than main's head: a pack change that
  needs a p1 change merged since the last release fails that gate until the next release, or a
  manual dispatch of `release.yml` publishes one.
- `install.sh --from-release` reaches only the newest 30 releases; a pruned tag is gone from
  the remote too.
- A partial publication is repaired by re-running the failed `release` job, which keeps the
  `choose` job's commit. A later scheduled run sees the release exists and does not repair it.
- The existing backlog shrinks by 100 a run, so it takes a few runs (or dispatches) to reach 30.

## Alternatives considered

- Keep a release per commit and only prune: removes the storage and clutter but keeps a release
  build after every merge, about 90 runner-minutes a day at the measured rate.
- Release only on a manual tag: the fewest builds, but `p1-update` and the pack-only gate then
  follow whenever someone remembers to tag.
- Prune by age instead of count: a quiet month would delete every rollback target.

## Evidence

- Counts from `gh release list` and `gh api repos/{owner}/{repo}/releases` (asset sizes and
  `download_count`) on 2026-10-12; release-run duration 4.8 minutes on average over the last 84
  successful runs (`gh run list --workflow release.yml`).
- `python3 scripts/test_release_publication.py`: the prune step deletes the 100 oldest of 140
  listed releases, never one of the newest 30 and never a tag outside `main-`.
- The first scheduled or dispatched run after merge shows the `choose`, `release` and `prune`
  jobs on Actions.
