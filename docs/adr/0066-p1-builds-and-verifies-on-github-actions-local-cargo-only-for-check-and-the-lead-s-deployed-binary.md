---
adr: 66
title: p1 builds and verifies on GitHub Actions; local cargo only for check and the lead's deployed binary
status: proposed
date: 2026-09-24
deciders: lead
supersedes: []
superseded_by: []
sources: [AGENTS.md, docs/lead-queue.md]
---
# ADR-0066: p1 builds and verifies on GitHub Actions; local cargo only for check and the lead's deployed binary

## Context

The owner ordered this on 2026-09-24 ~23:00 (XO to p1-lead, %51): "stop building p1 locally except `cargo check -p` and the lead's one deployed-binary rebuild; local rustc slots and RAM go to the Iris run. p1 is public → Actions minutes are free; runners are x86_64 (artifacts run here)."

The machine has 7 GB RAM, one shared cargo target root and a machine-wide two-slot rustc semaphore (ADR-0060). A local full gate holds a slot for 20+ minutes and competes with the Iris run, while the same script already runs in this repository's CI (`.github/workflows/ci.yml`, the run `scripts/push-main.sh` waits for). The local gate re-proved remotely what the remote run could prove, at the cost of the run it was meant to protect, and `p1-host`'s debug binary was a separate local build on top of that.

The owner restated and sharpened this on 2026-09-25 02:40: CI (push plus `scripts/ci-build.sh`) is the build farm; local cargo is ONLY `cargo check -p <crate>` plus the lead's deployed-binary rebuild; any local build target goes on the SSD as `CARGO_TARGET_DIR=~/.cache/cargo-target/<task>`, one per task, `CARGO_BUILD_JOBS=2`, at most two concurrent rustc, admitted only above the SSD floor (8 GiB free to keep building, 12 GiB to admit a new build), and the target's owner deletes it at task end. That supersedes the `/mnt/build` HDD default this section previously carried.

## Decision

p1 builds and verifies on GitHub Actions. A push to any `task/**` branch runs `scripts/gate.sh` and `cargo build -p p1-host` (debug, as deployed) in `.github/workflows/build.yml` and uploads the `p1` binary, its sha256 and the gate log as the `p1-build` artifact. `scripts/ci-build.sh [<branch>] [--no-download] [--wait-only]` pushes that branch, waits for the run of exactly that commit, prints the summary, downloads the artifact to `ci-artifacts/<sha>/` and exits 0 (success, digest verified with `sha256sum -c`), 1 (the run failed or was cancelled, or the digest does not verify) or 2 (usage or tooling error — a missing or failing local tool is always 2, never 1). Local cargo is ONLY `cargo check -p <crate>`, plus the lead's deployed-binary rebuild under the SSD target rule above; a full local gate is run only when Actions cannot be used.

Run identity is one workflow of one commit: only `.github/workflows/build.yml` runs of the exact headSha count, and only a run this invocation caused. In `push` mode (default) that is `event=push`; a push that moves the branch accepts only a run id that did not exist before the push, while a push that moves nothing (the same commit is already on the branch, so GitHub creates no run) waits for that commit's own run. A detached HEAD is refused whether or not a branch was named, because the push must come from the branch's checkout. In `dispatch` mode (a repository whose workflow does not run on push) the script reuses a `build.yml` run for the commit that is queued, running or green, and otherwise starts one with `gh workflow run build.yml --ref <branch>` and accepts only a run id that did not exist before the dispatch, so a concurrent dispatch or a newly visible older dispatch cannot be pinned. `--wait-only` neither pushes nor dispatches. The artifact is accepted only when `p1`, `p1.sha256` and `gate.log` are all present and `sha256sum -c p1.sha256` passes inside the artifact directory, so the digest is bound to the file it names.

The branch cache is `Swatinem/rust-cache` under one shared key without a branch name (each branch restores its own entry, warm from its second push), and the cross-branch fallback is the cache `ci.yml` already saves on main: `build.yml` restores `cargo-<runner os>-<Cargo.lock hash>` with `actions/cache/restore` and **does not save it**, because a workflow run may read the default branch's caches while a task branch must not write another multi-GB copy. `build.yml` deliberately does not run on main.

## Consequences

- The two local rustc slots and the RAM stay with the Iris run; the machine no longer spends 20+ minutes re-proving what CI proves.
- "Green" now means this: the build.yml run of exactly the pushed commit is `success`, and the downloaded `p1` passes `sha256sum -c` against the uploaded `p1.sha256` with `gate.log` beside it. Workers use `scripts/ci-build.sh` as their build/verify step and it is the only build they run.
- Feedback is slower than a warm local build (minutes instead of seconds) and needs the network; the artifact is a debug binary, not the release binary (`p1-host` is built debug because that is what is deployed).
- `ci.yml` is untouched, so the main/PR behaviour and the run `push-main.sh` looks for are unchanged, and a PR does not run the gate twice; `build.yml` never adds a run on main, so `push-main.sh`'s `head -1` headSha pick stays unambiguous.
- Local builds are now `cargo check -p` plus the lead's rebuild, each with its own SSD target that its owner deletes at task end, so the free-space and slot rules are followed rather than judged per build.
- A run of another workflow, of another event, or of an earlier invocation is never accepted, and a local tool failure is always exit 2 rather than being misread as a red run; the cost is one extra `gh run list` (the pre-push snapshot) and a `git ls-remote` per invocation, and the failure mode of a pushed commit with no matching run is the timeout (exit 2), not another run's verdict.
- Caches live in GitHub's per-repository cache store, subject to its 10 GB eviction policy: the restore-only main fallback adds no entries (the store was already near the limit), and the first push of a fresh branch still recompiles what main's `cargo-*` entry does not hold.
- The lead still owns the deployed binary's rebuild; that stays local by owner order.

## Alternatives considered

- Seeding the rust-cache key from main by adding `push: branches: [main]` to `build.yml` with `save-if` limited to main: rejected because it adds a second run per main commit and `scripts/push-main.sh` picks the newest run for that headSha, so a build.yml failure after a green gate (or an upload failure) would be read as main's CI verdict, and main would gate twice. The restore-only fallback warms a new branch without either effect.
- Keep the full local gate and add the CI build: doubles the work and keeps the slot contention the order was issued to remove.
- Remote sccache (shared compile cache in the Actions cache or object storage): cuts rustc work but still compiles locally, so it does not free the slots while the Iris run is live; a possible complement later.
- A self-hosted GitHub runner on spare hardware: unlimited minutes and a local cache, but it needs a machine and a runner token from the owner.
- A rented build VPS (Hetzner and similar): fast and simple, but spends money and needs repository access on the box — an owner decision, not a default.
- Oracle Cloud free ARM tier: wrong architecture for binaries that must run on this x86_64 laptop.

## Evidence

Measured on 2026-09-24 with `scripts/ci-build.sh` on `task/cloud-builds`.

- Cold run 36059056426 (created 21:04:14Z, finished 21:09:18Z; `run_duration_ms` 304000 = 5m04s; gate step 268s, `cargo build -p p1-host` 13s): the cache step logged `No cache found.` and then `Saving cache`, saving `v0-rust-p1-build-Linux-x64-6ff13d87-892cc3d9` (332 MB) under `refs/heads/task/cloud-builds`. `ci-build.sh` exited 0 and downloaded `ci-artifacts/4b95000cc24b8a57bc2faa20f3e97c0877d21ac0/` (`p1` 95321144 bytes, `p1.sha256`, `gate.log`), printing `sha256 (downloaded)` = `sha256 (uploaded)` = 791e2a461a677347a35fed4f182312ae2111811250d4ae73dcebf646877f6741, which matches a local `sha256sum` of the same file.
- Cached second push (run 36059849406, created 21:11:27Z, finished 21:15:03Z; `run_duration_ms` 216000 = 3m36s; gate step 186s, build 9s): the cache step logged `Cache hit for: v0-rust-p1-build-Linux-x64-6ff13d87-892cc3d9`; 88s (29%) faster than cold, and the downloaded `p1` again hashed to the same value, so the binary is reproducible for the same code.
- Red run 36060426614 on the throwaway branch `task/cloud-builds-red` (77a6f74, a deliberately unformatted function): `ci-build.sh` printed the run summary and the failed-step log tail (the `cargo fmt` diff) and exited 1; the run's `p1-build` artifact holds only `gate.log` (374 B). The branch was then deleted (`git ls-remote --heads origin task/cloud-builds-red` is empty).
- The uploaded gate log ends `== gate: GREEN`, so the CI gate is `scripts/gate.sh` — the same script as local.
- `python3 scripts/test_ci_build.py` (44 tests, stubbed `git`/`gh`/`sleep`/`sha256sum`, no network, polling driven by canned answer sequences rather than by time) covers argument parsing, the `main` and detached-HEAD refusals (with and without a branch argument), the 0/1/2 mapping, the run identity and correlation and the artifact checks: a run of another event or one that existed before the push is not accepted, a push that moves nothing waits for that commit's own run, a dispatch accepts only the run id it started, failure and cancellation exit 1 with the failed-step log tail, a digest mismatch and a manifest naming another file exit 1, and a failing `mktemp`/`mkdir`/`find`/`mv`/`sha256sum`/`cut`/`sleep`, a missing tool, a missing `gate.log` and a failed download exit 2.
- `shellcheck scripts/ci-build.sh` and `bash -n scripts/ci-build.sh` are clean.
- Actions limits of this repository and account (`gh api`): `repos/5omeOtherGuy/phaseone/actions/permissions` → `enabled`, `allowed_actions: all`, `sha_pinning_required: false`; `.../actions/permissions/workflow` → `default_workflow_permissions: read` (which is what `build.yml` asks for); `actions/runs/<id>/timing` → `billable.UBUNTU.total_ms: 0` for both runs, i.e. a public repository bills no minutes; `actions/cache/usage` → 10291930227 bytes of the 10 GiB repository cache limit in 8 caches (so the new per-branch cache has little headroom and older entries will be evicted); the account billing endpoints (`users/5omeOtherGuy/settings/billing/actions`, `orgs/5omeOtherGuy/...`) answer 404 without the `user` scope, so no account-level limit was readable.
- Re-check: `scripts/ci-build.sh <task-branch>` on any `task/**` push, and the run's page at `https://github.com/5omeOtherGuy/phaseone/actions`.
