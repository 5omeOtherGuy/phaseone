---
adr: 77
title: Builds on the stream boxes
status: proposed
date: 2026-09-25
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [DECISIONS.md, docs/adr/0066-p1-builds-and-verifies-on-github-actions-local-cargo-only-for-check-and-the-lead-s-deployed-binary.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, scripts/gate.sh, scripts/rustc-serial, scripts/local-cargo-config.sh, scripts/module-toolchain.sh, scripts/build-modules.sh, .github/workflows/ci.yml, .github/workflows/build.yml]
---
# ADR-0077: Builds on the stream boxes

## Context

The WebAssembly migration (ADR-0071, DECISIONS.md D22) runs in parallel streams, each led from
its own EC2 box. ADR-0066 makes GitHub Actions the build farm and allows local cargo only for
`cargo check -p` and the lead's deployed-binary rebuild; ADR-0071 says builds run on "the
dedicated EC2 machine". With several streams building at once, one CI queue cannot give every
stream its own build and test loop.

A "Dedicated build runner" ADR was planned: a self-hosted runner registered with GitHub. Review
finding F1 showed that such a runner exposes the owner's account to fork PRs, whose workflows
would execute on it. The owner decided (2026-09-25, through XO) that the stream boxes are lead
machines that also build: every stream runs `scripts/gate.sh`, cargo builds and tests on its own
box, and GitHub Actions on GitHub-hosted runners is only the merge gate. The dedicated runner ADR
is dropped and never landed.

## Decision

On each stream's box, `scripts/gate.sh`, cargo builds and cargo tests run locally through the
box-wide build semaphore: the bootstrap's `~/.cargo/config.toml` sets a rustc-wrapper that is
byte-identical to `scripts/rustc-serial` and shares its lock directory (`P1_BUILD_LOCK_DIR`) and
slot count (`P1_RUSTC_SLOTS`), so a worktree's `.cargo/config.toml` written by
`scripts/local-cargo-config.sh` keeps the same semaphore. Each worktree has its own cargo target,
never shared (D20); the land step deletes a slice's target together with its worktree.

GitHub Actions on GitHub-hosted runners is only the merge gate: the required `gate` check
(`.github/workflows/ci.yml`) and the `task/**` build artifact (`.github/workflows/build.yml`). No
self-hosted runner is registered with GitHub.

Which gate variant runs where:

| Where | Gate variant |
|---|---|
| Stream boxes | The full `scripts/gate.sh` with bubblewrap: the bwrap probe succeeds and every sandbox test runs. |
| GitHub-hosted runners (`gate` check, `build.yml`) | The same `scripts/gate.sh`; the bubblewrap tests skip, printing `SKIP: bwrap unusable here` when the probe fails (`crates/p1-tool-shell/tests/sandbox.rs`, `crates/p1-host/tests/sandbox.rs`). |
| The owner's workstation | Unchanged (ADR-0066): local cargo only `cargo check -p` and the lead's deployed-binary rebuild. |

Both gate variants also run the module steps: `scripts/module-toolchain.sh --check` and
`scripts/build-modules.sh --all`, a no-op while `modules/` has no package.

This amends ADR-0066 for the boxes only; its workstation rule is unchanged. It amends
ADR-0071's build consequence: "Builds run on the dedicated EC2 machine" now reads "Builds run on
each stream's box". It supersedes nothing. It replaces the dropped "Dedicated build runner" ADR.

## Consequences

- Every stream has its own build and test loop; a slice reaches its PR with a green box gate,
  and CI confirms it rather than being the first place it builds.
- The sandbox tests are only fully exercised on the boxes. A green CI gate does not prove the
  bubblewrap paths; the box gate is the evidence for them.
- The box-wide semaphore keeps concurrent worktrees from exhausting a box's memory; builds wait
  for a slot instead of failing.
- Per-worktree targets cost disk on each box; deleting the target with the worktree at landing
  keeps it bounded.
- No runner registered with GitHub means fork PRs never execute on owner-controlled machines
  (closes F1).
- Both the box and CI toolchains must carry the `wasm32-wasip2` target; the workflows install it
  and `scripts/module-toolchain.sh --check` fails when it is missing.

## Alternatives considered

- A dedicated self-hosted build runner registered with GitHub: dropped, because it exposes the
  owner's account to fork PRs (F1).
- CI-only builds as ADR-0066 has it: rejected for the boxes, because every stream needs its own
  build and test loop and CI is a queue.

## Evidence

- The bwrap probe succeeds on box wasm-s0 (the sandbox tests run rather than skip).
- `scripts/module-toolchain.sh --check` and `scripts/gate.sh` pass on box wasm-s0 and in the
  PR's `gate` check on a GitHub-hosted runner (the lead adds the run ids and the merge commit at
  acceptance).
