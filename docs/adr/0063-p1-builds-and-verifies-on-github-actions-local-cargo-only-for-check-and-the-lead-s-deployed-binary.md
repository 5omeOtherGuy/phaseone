---
adr: 63
title: p1 builds and verifies on GitHub Actions; local cargo only for check and the lead's deployed binary
status: proposed
date: 2026-09-24
deciders: lead
supersedes: []
superseded_by: []
sources: [AGENTS.md, docs/lead-queue.md]
---
# ADR-0063: p1 builds and verifies on GitHub Actions; local cargo only for check and the lead's deployed binary

## Context

The owner ordered this on 2026-09-24 ~23:00 (XO to p1-lead, %51): "stop building p1 locally except `cargo check -p` and the lead's one deployed-binary rebuild; local rustc slots and RAM go to the Iris run. p1 is public → Actions minutes are free; runners are x86_64 (artifacts run here)."

The machine has 7 GB RAM, one shared cargo target root and a machine-wide two-slot rustc semaphore (ADR-0060). A local full gate holds a slot for 20+ minutes and competes with the Iris run, while the same script already runs in this repository's CI (`.github/workflows/ci.yml`, the run `scripts/push-main.sh` waits for). The local gate re-proved remotely what the remote run could prove, at the cost of the run it was meant to protect, and `p1-host`'s debug binary was a separate local build on top of that.

## Decision

p1 builds and verifies on GitHub Actions. A push to any `task/**` branch runs `scripts/gate.sh` and `cargo build -p p1-host` (debug, as deployed) in `.github/workflows/build.yml` and uploads the `p1` binary, its sha256 and the gate log as the `p1-build` artifact. `scripts/ci-build.sh [<branch>] [--no-download] [--wait-only]` pushes that branch, waits for the run of exactly that commit (as `scripts/push-main.sh` does for main), prints the summary, downloads the artifact to `ci-artifacts/<sha>/` and exits 0 (success, sha256 verified), 1 (the run failed or was cancelled) or 2 (usage or tooling error). Local cargo is limited to `cargo check -p <crate>` and the lead's deployed-binary rebuild; a full local gate is run only when Actions cannot be used.

## Consequences

- The two local rustc slots and the RAM stay with the Iris run; the machine no longer spends 20+ minutes re-proving what CI proves.
- "Green" now means this: the gate run of exactly the pushed commit is `success` and the downloaded `p1` matches the uploaded `p1.sha256`. Workers use `scripts/ci-build.sh` as their build/verify step and it is the only build they run.
- Feedback is slower than a warm local build (minutes instead of seconds) and needs the network; the artifact is a debug binary, not the release binary (`p1-host` is built debug because that is what is deployed).
- `ci.yml` is untouched, so the main/PR behaviour and the run `push-main.sh` looks for are unchanged, and a PR does not run the gate twice.
- Caches live in GitHub's per-repository cache store, subject to its 10 GB eviction policy, and a cold run pays the full dependency compile.
- The lead still owns the deployed binary's rebuild; that stays local by owner order.

## Alternatives considered

- Keep the full local gate and add the CI build: doubles the work and keeps the slot contention the order was issued to remove.
- Remote sccache (shared compile cache in the Actions cache or object storage): cuts rustc work but still compiles locally, so it does not free the slots while the Iris run is live; a possible complement later.
- A self-hosted GitHub runner on spare hardware: unlimited minutes and a local cache, but it needs a machine and a runner token from the owner.
- A rented build VPS (Hetzner and similar): fast and simple, but spends money and needs repository access on the box — an owner decision, not a default.
- Oracle Cloud free ARM tier: wrong architecture for binaries that must run on this x86_64 laptop.

## Evidence

- Cold run 2026-09-24 21:04:14Z–21:09:18Z (5m04s), run 36059056426 on `task/cloud-builds` at 4b95000: `ci-build.sh` exited 0, downloaded `ci-artifacts/4b95000cc24b8a57bc2faa20f3e97c0877d21ac0/` (`p1` 95321144 bytes, `p1.sha256`, `gate.log`) and printed `sha256 (downloaded)` = `sha256 (uploaded)` = 791e2a461a677347a35fed4f182312ae2111811250d4ae73dcebf646877f6741, matching a local `sha256sum` of the same file.
- The uploaded gate log ends `== gate: GREEN`, so the CI gate is `scripts/gate.sh` — the same script as local.
- `python3 scripts/test_ci_build.py` (21 tests, stubbed `git`/`gh`/`sleep`, no network) covers argument parsing, the `main`/detached-HEAD refusals and the 0/1/2 mapping: failure and cancellation exit 1 with the failed-step log tail, unverified sha exits 1, push/`gh`/download failures and a missing run exit 2.
- `shellcheck scripts/ci-build.sh` and `bash -n scripts/ci-build.sh` are clean.
- Re-check: `scripts/ci-build.sh <task-branch>` on any `task/**` push, and the run's page at `https://github.com/5omeOtherGuy/phaseone/actions` (public repository: Actions minutes and artifact storage are free).
