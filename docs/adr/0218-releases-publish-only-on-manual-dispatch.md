---
adr: 218
title: Releases publish only on manual dispatch
status: accepted
date: 2026-10-13
deciders: owner
supersedes: [216]
superseded_by: []
sources: [ADR-0065, ADR-0206, ADR-0216, issue #934]
---
# ADR-0218: Releases publish only on manual dispatch

## Context

ADR-0216 replaced ADR-0065's release after every green gate on `main` with a daily scheduled
release and pruning to the newest 30. A development session then set a timer for the next
scheduled release and waited instead of working. Owner (2026-10-13): "Revert your changes,
releases will be triggered manually by us when we need them", and "we are in development and
not even close to a release ready product."

## Decision

`release.yml` runs only on `workflow_dispatch`. A dispatch publishes the newest commit whose gate
ran green for a push to `main`, unless that commit's release already exists. Nothing publishes on
a schedule or after a merge, and nothing prunes. ADR-0065's tag, assets, idempotent publication
and installer hold.

## Consequences

- No release appears by itself: a session that needs a newer installed p1 asks for a dispatch,
  or builds one with `scripts/install.sh --local`; it never waits for a release.
- `p1-update` and the pack-only gate (ADR-0206) follow the latest dispatched release, which can
  be older than `main`; a pack change that needs a newer p1 fails that gate until someone
  dispatches a release.
- Releases accumulate one per dispatch; the 148 releases ADR-0216's prune runs deleted are not
  restored.

## Alternatives considered

- Restore ADR-0065's release after every green gate: the owner wants releases only on request.
- Keep the schedule without pruning: a schedule is what a session waited on.

## Evidence

`python3 scripts/test_release_publication.py`; the first dispatch after merge shows the `choose`
and `release` jobs on Actions.
