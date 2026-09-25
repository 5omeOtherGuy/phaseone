---
adr: 65
title: p1 installs from its release channel into a prefix and the binary names its commit
status: proposed
date: 2026-09-24
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0065: p1 installs from its release channel into a prefix and the binary names its commit

## Context

Until now the only way to run p1 was to clone the repository and `cargo build` it: there
was no install, no update path, and nothing that said which commit a binary came from.
That is fine for the agents working in this repo and wrong for anyone else — the 7 GB-class
machine normally forbids a release build on a workstation, so a user cannot be asked to
produce the shipping binary. The single local exception is the guarded `--local` fallback
defined below.

The repository is public (`github.com/5omeOtherGuy/phaseone`), the gate already runs on
every push to `main` and is the single definition of green (ADR-0011), and p1 has no
version numbering beyond `0.0.1` — the commit is the only real identity it has. D7
recorded "there are no other workflows": the release workflow added here is not a check,
it runs only after the gate succeeded and can make no commit green or red, so ADR-0011
stands unchanged.

Two directories exist at runtime and they must not be confused: the user's
`$HOME/.config/p1` (auth store and local overrides; ADR-0040, ADR-0044, ADR-0061) and the
shipped data. `crates/p1-host/src/main.rs` already resolves the shipped data as
`<exe dir>/../share/p1/environments` with `routes/` and `profiles/` as siblings, so an
installed layout is implied by the code but was never produced.

## Decision

p1 is installed from a published release, and the binary says which commit it is.

- After a **successful `gate` run on `main`**, the release workflow builds `p1` in release
  profile on a GitHub runner (the normal release build path) and publishes
  the GitHub Release tagged `main-<12-char short sha>` with four assets — `p1-linux-x86_64`,
  `p1-linux-x86_64.sha256`, `p1-share.tar.gz` (top-level `environments/`, `routes/`,
  `profiles/`), `p1-share.tar.gz.sha256` — marked latest. The tag is the commit: the
  workflow resolves the tag's commit (`git ls-remote`) and refuses a mismatch, **creates**
  the tag at the commit the gate ran on through `gh release create --target`, so
  publication never depends on the tag already existing; a re-run checks the complete asset
  set, a missing asset is uploaded with replacement, and a tag without a release is
  completed rather than treated as published.
- `scripts/install.sh [--latest | --from-release TAG | --local] [--prefix DIR]` installs
  `<prefix>/bin/p1` (0755), the share data at `<prefix>/share/p1/{environments,routes,profiles}`
  — exactly what `main.rs` looks for — plus `<prefix>/share/p1/install.sh` and
  `<prefix>/bin/p1-update` (which runs the installed installer with
  `--latest --prefix <prefix>`, so updating needs no checkout). Both checksums are verified
  before the prefix is touched. The binary, updater and share are staged first, validated
  (the share contains only regular files and directories under the three shipped roots),
  and then committed by same-filesystem renames with recoverable rollback across all three;
  a failed install therefore leaves the previous binary, updater and share intact, and a
  signal during the commit rolls back and ends the script instead of resuming a half-swapped
  install. The rollback copies use one fixed slot per prefix, named in the failure message
  when a restore fails. Installing the release already present is a no-op unless `--force`
  is given — `main-<sha>` is recognized from the `p1 --version` line, and the latest release
  is resolved with one tag lookup, not a download. Explicit `--from-release` permits
  downgrades; the updater follows the release marked latest.
- The installer needs bash, `curl` (or `gh`, which is an optimization: a `gh` that fails or
  is unauthenticated falls back to the public release URL), `sha256sum`, and **python3**:
  the share tarball is validated and extracted with `tarfile` and the PEP 706 `filter=`
  kwarg, and the prospective prefix is checked in Python. The interpreter is python3 3.12,
  or 3.8.17 / 3.9.17 / 3.10.12 / 3.11.4+ with the security backports; it is probed once
  before any install work, and a missing or older one is refused naming itself rather than
  as a statement about the archive or the prefix.
- `p1 --version` prints `p1 <CARGO_PKG_VERSION> (<sha> <date>)`. `crates/p1-host/build.rs`
  takes the sha from `P1_GIT_SHA`, else `git rev-parse --short=12 HEAD`, else `unknown`, and
  the date from `P1_BUILD_DATE`, else `SOURCE_DATE_EPOCH`, else `unknown` — never the wall
  clock, so two builds of one commit print the same string.
- `--local` is the one admitted local fallback for a build that cannot run in the cloud. It
  builds the current checkout with `cargo build --release --locked -p p1-host` into an
  absolute `$CARGO_TARGET_DIR`, or `$HOME/.cache/cargo-target/p1-release` when that variable
  is unset. The resolved target must be below `$HOME/.cache/cargo-target`, on an ext4
  filesystem, and have at least 12 GiB free. Cargo always receives two jobs and this
  checkout's `scripts/rustc-serial` wrapper. The repository's own `target/` and the retired
  internal HDD are never used.
- The installer never reads, writes or deletes anything under
  `${XDG_CONFIG_HOME:-$HOME/.config}/p1`; it refuses a prefix whose prospective `bin` or
  `share` path resolves into that tree.
- `scripts/fanout.py` finds the binary as `P1_BIN`, else `p1` on `PATH` (the installed
  one), else the current `../phaseone-target/debug/p1` fallback of a development checkout.

## Consequences

- Installing or updating needs no Rust toolchain, no repository and no compile on the
  user's machine; `p1-update` is the whole update path.
- A user's shell must have `<prefix>/bin` on `PATH`; the installer warns when it does not,
  and nothing else edits shell profiles.
- One tag per green push to `main` accumulates (`main-<sha>`), and every release is marked
  latest, so "latest" tracks `main`, not a stable version. Nothing prunes old releases.
- Publishing is idempotent per commit: a red gate publishes nothing; a re-run checks the
  tag's commit and all four assets, repairing an incomplete release or leaving a complete
  one alone.
- The sha256 assets catch a truncated or corrupted download, not a compromised release;
  the release is trusted because it was built by CI from a commit the gate accepted.
- The data dir and the binary can drift apart if a user copies files by hand; updating by
  hand is not a supported path.
- Open: the host's installed-layout lookup (`crates/p1-host/src/main.rs`'s
  `$P1_CONFIG_DIR` / `$P1_ENVIRONMENTS_DIR` / exe-relative search, `routes.rs`, `models.rs`)
  is still verified only by hand — it needs a cargo build, which this worktree could not
  run. `scripts/test_install.py` covers the installer's output layout, not the host's
  search order over it. Tracked as the open item of the round-1 install review.
- Normal release builds happen on GitHub runners. The only admitted workstation release build
  is `scripts/install.sh --local`, guarded by the absolute per-task SSD target, ext4, 12 GiB
  free, two-job, and `rustc-serial` rules above.

## Alternatives considered

- **`cargo install` / crates.io.** Needs a published crate, a toolchain and a compile on
  the user's machine, and the share data would still need a home — more moving parts than
  a four-asset release, and it contradicts the local build rules.
- **A binary asset only, with the share data expected from a checkout.** A user would have
  to clone the repository for `environments/`, which is exactly what this decision removes.
- **`v<crate version>` tags.** Every commit is still `0.0.1`, so the tag would not name a
  build; the commit is the identity p1 actually has.
- **A date or a CI run number as the version.** Says nothing about which code is in the
  binary, and cannot be re-derived from a checkout.
- **Wall-clock build date.** Breaks reproducible builds; `SOURCE_DATE_EPOCH` and the
  commit's own date answer the same question.
- **Committing the release binary to the repository.** A binary in git, plus the same
  release build on a workstation that `AGENTS.md` forbids.

## Evidence

- Files: `.github/workflows/release.yml`, `scripts/install.sh`, `scripts/update.sh`,
  `crates/p1-host/build.rs`, `crates/p1-host/src/cli.rs` (`version`),
  `scripts/test_install.py`, `scripts/test_fanout.py`, `README.md`, `AGENTS.md`.
- `python3 scripts/test_install.py -v` (49 tests, no network): a fixture release with stub
  `gh`, `curl` and `cargo` on `PATH` covers the successful install and its modes, gh
  fallback (including a `gh` that fails after writing a truncated asset), checksum refusals
  (nothing installed, the previous install intact), unsafe archive members, protected
  config prefixes, same-release no-op/force and downgrade behavior for `--latest`,
  `--from-release main-<sha>` and the `p1-update` wrapper, a missing or older python3
  refused by name, failure-atomic binary/share/updater rollback (including a colon-containing
  prefix) with a rolled-back SIGTERM during the commit, a failed restore reported with its
  fixed-slot leftovers and retained across another failure, release-workflow repair/cache/tag-
  creation assertions, and `--local` default/absolute override, relative/outside-root refusal,
  two-job and rustc-wrapper enforcement, fixed 12-GiB boundary, exact ext4 admission,
  non-ext4 refusal, and failed or malformed filesystem/free-space probe refusal.
- `python3 scripts/test_fanout.py -v`: `$P1_BIN` over `p1` on `PATH` over the debug
  fallback, and a job that runs the `p1` found on `PATH` without `P1_BIN`.
- `bash -n scripts/install.sh scripts/update.sh`; `shellcheck scripts/install.sh scripts/update.sh`.
- The release assets were produced with the workflow's own commands in a temp directory
  (`install -m 0755` a stand-in binary, `tar -czf … environments routes profiles`,
  `sha256sum`), then installed with `scripts/install.sh --from-release` against a stub `gh`
  serving them: both checksums verified, the binary and the real share data landed in the
  documented layout, and `<prefix>/bin/p1-update` ran the installed installer.
- `crates/p1-host/build.rs` compiled on its own with
  `CARGO_MANIFEST_DIR=<crate> rustc --edition 2024 -o /tmp/buildrs crates/p1-host/build.rs`
  and run: with no environment it emitted the checkout's 12-char sha and `unknown`; with
  `P1_GIT_SHA` and `P1_BUILD_DATE` it emitted those; with `SOURCE_DATE_EPOCH=1758681600` it
  emitted `2025-09-24`; with a blank sha and a non-numeric epoch it fell back to git and
  `unknown`. The `--version` assertions were exercised standalone against
  `p1 0.0.1 (1a2b3c4d5e6f 2026-09-24)` (accepted), `p1 0.0.1 (unknown unknown)` (accepted) and
  the old `p1 0.0.1` form (rejected).
- The Rust side of this change was **not** run through cargo in this worktree: the root
  filesystem held less free space than the 12 GiB build threshold, so no cargo command was
  allowed. Re-check with `cargo test --locked -p p1-host --test cli` and
  `cargo test --locked -p p1-host --test host help_and_version` on a machine with room.
