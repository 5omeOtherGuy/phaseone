---
adr: 97
title: GitHub-hosted CI provisions bubblewrap and requires the sandbox suites
status: accepted
date: 2026-09-29
deciders: owner+lead
supersedes: [77]
superseded_by: []
sources: [.github/workflows/ci.yml, .github/workflows/build.yml, scripts/ci-bwrap.sh, scripts/gate.sh, .luna/issues/381.md, crates/p1-module-tests/tests/installed_release.rs]
---
# ADR-0097: GitHub-hosted CI provisions bubblewrap and requires the sandbox suites

## Context

ADR-0077 made GitHub-hosted runners a gate variant that skips the bubblewrap boundary
suites: the probe fails, the suites print `SKIP: bwrap unusable here` and return, and a green
`gate` check therefore does not prove the sandbox paths. Issue #381 records the cost of that
rule for the installed-release cases (D-XO-47): on those runners both cases returned after
loading the package and skipped the headless run, the provider exchanges, the journal
assertions and the corrupted-package refusal — the check was green while the boundary it
exists to prove was unproven. Codex review of the #381 fix (PR #465) also showed that the
replacement check grepped for a marker libtest captures on a passing test's stderr, so it did
not observe a skipped suite at all.

## Decision

GitHub-hosted runners that run the gate install bubblewrap (`scripts/ci-bwrap.sh`), lift the
AppArmor restriction on unprivileged user namespaces, and probe the same bwrap invocation the
gate and the Rust suites probe; a failed probe fails the job. Each gate partition then rejects
a skipped boundary suite observably: it greps the gate's own
`bubblewrap: unusable on this CI runner` message (printed by gate.sh itself, so libtest's
stderr capture cannot hide it) and the `SKIP: bwrap unusable here` marker, and the test steps
run with `--nocapture` so a passing suite's marker reaches the log too. The bubblewrap suites are
therefore required on GitHub-hosted runners, not only on the stream boxes. This supersedes
ADR-0077's rule that the GitHub-hosted gate variant skips them; ADR-0077's build placement
(stream boxes build through the shared semaphore, GitHub Actions is the merge gate) stands.

## Consequences

A green PR `gate` now proves the bubblewrap boundary on the same machine as the rest of the
gate, so a sandbox regression cannot pass review. The gate is longer and one more apt package
and sysctl run per job; a runner where bubblewrap cannot be made usable fails rather than
passing with a skip. The stream boxes keep the full gate as ADR-0077 describes, and the
`P1_REQUIRE_BWRAP=1` box setting still turns a skip into a failure there.

## Alternatives considered

Keep ADR-0077's CI skip and rely on the stream-box gate for boundary evidence: rejected,
because a PR's required check is what merges, and #381 showed the cases silently degrading.
Keep the skip but count or fail on the marker alone: rejected, because libtest captures a
passing suite's stderr, so the marker is absent from the log unless the step runs with
`--nocapture`, and a weaker provisioning probe can pass while the gate probe fails.

## Evidence

Issue #381 and D-XO-47 prescribe installing bubblewrap and lifting the AppArmor restriction in
the `test (p1-module-tests, …)` jobs; PR #465 review finding `build.yml:92` showed the
captured-stderr gap. `scripts/ci-bwrap.sh` provisions and probes; `scripts/test_ci_bwrap.py`
pins the provisioning, the failed-probe exit and the workflows' detection of the gate's own
unusable message; `scripts/test_gate.py` keeps the frozen CI-skip behaviour of `gate.sh`
itself.
