# p1 — instructions for agents and humans

p1 is a lean, modular Rust coding harness. Direction: `docs/design/` (start at
`README.md`). Settled choices: `DECISIONS.md`. Work items: GitHub Issues.
`STATUS.md` is the lead session's resume record — only the lead edits it.

Several agents work in this repository at the same time. Everything below exists to
make that fast and conflict-free.

## Swarm protocol

- **One worktree per task.** Never run two agents in one checkout.
  `scripts/new-worktree.sh <task-slug>` creates `../phaseone-<task-slug>` on a fresh
  `task/<task-slug>` branch with its own seeded cargo target dir (see Build).
- **Work items are issues.** Claim one by assigning yourself and moving the label
  `ready` → `in-progress`. Stuck? Add `blocked` and say what you need in a comment.
  Only owner decisions carry the `owner` label.
- **Own your paths.** Change only the paths your task owns. Do not reformat, rename
  or "tidy" anything else — another agent owns it.
- **Trunk-based.** Commit often on `task/<issue>-<slug>`, push it, and merge into
  `main` as soon as `scripts/gate.sh` is green. A pull request with auto-merge is the
  default route (`gh pr create --fill && gh pr merge --auto --squash --delete-branch`);
  merging locally and pushing `main` is equally fine. No review gate, no long-lived
  branches, no branch protection.
- **A small diff is a mergeable diff.** If a conflict needs a judgement call, keep
  both sides working and re-run the gate.
- **Shared files** (`AGENTS.md`, `DECISIONS.md`, `Cargo.toml`, `Cargo.lock`,
  `scripts/`, `.github/`): keep edits minimal and merge `main` immediately before
  touching them. `DECISIONS.md` is append-only — never rewrite an existing row.
- **Never `git add -A`; commit by explicit path.** Never force-push `main`.

## Workers

Jobs are dispatched with `scripts/fanout.py <jobs.json>`; it starts them up to a machine-wide
pool bound and prints one JSON summary when the batch ends. `"runner": "p1"` runs the job
with p1 itself, sandboxed, and leaves a run directory holding the session journal, the
agent's stdout/stderr and `report.json` (from `scripts/run-report.py`). The lead still
verifies independently and records accepted runs in `docs/dogfood/runs.jsonl`.

## Gate

`scripts/gate.sh` is the only required check: `cargo fmt --check`,
`clippy -D warnings`, all tests, core isolation. CI runs exactly the same script.
It must be green before a merge; it is not required for every intermediate commit.
After merging into `main`, push with `scripts/push-main.sh`: it waits for the CI run of exactly
that commit. CI differs from a workstation (no bubblewrap, slower start-up) — a green local gate
is not a green CI.

## Decisions

A decision that changes an interface, a dependency rule, a workflow rule, or that reverses
an earlier decision gets an Architecture Decision Record in `docs/adr/` — start one with
`scripts/adr.py new "Title"`. Keep it `proposed` until the change is merged, then set it
`accepted` (or `rejected`). Never edit an accepted ADR except its `status` and
`superseded_by`; reverse it with a NEW ADR that supersedes it (`--supersedes N`). Small
choices stay in commit messages. `scripts/adr.py check` runs in the gate.

## Hard rules

- No sudo, no package installs. Ask in an issue.
- Never read, print, log, commit or put into fixtures any credential, token,
  Authorization header, private prompt or raw authenticated traffic.
- No live network in unit/conformance tests. No real user data directories — use
  `tempfile`/scratch dirs. No sleep-based timing assertions; use fake time or
  explicit synchronisation.
- `/home/phaseonebig/projects/iris-agent` and `iris-agent-clean` are read-only donors.
  Copy code into p1 and adapt it; name the donor path in the commit message.
- Never delete, weaken or skip a frozen acceptance test or its fixtures. If a check
  contradicts the spec, leave it failing and explain.
- Keep dependencies few. Ask before adding a crate that is not already in the workspace.

## Build

- Small SSD, 7 GB RAM. `scripts/new-worktree.sh` sets each worktree up with
  `scripts/local-cargo-config.sh` (untracked `.cargo/config.toml`): its OWN `target/`,
  seeded with hardlinks of the already-built third-party dependencies (no extra disk, no
  rebuild), and `scripts/rustc-serial` as rustc wrapper — a machine-wide semaphore that lets
  at most two rustc processes run at once, however many agents build. A build that seems to
  hang is waiting for a slot: wait, do not kill it, do not work around it.
- Never point a worktree at another checkout's target dir and never set `CARGO_TARGET_DIR`:
  cargo cannot tell two worktrees' `p1-*` crates apart and links stale artifacts (D20).
- Build as little as possible: `cargo check -p <crate>` / `cargo test -p <crate> <filter>`
  while iterating; the full gate once, at the end. No `cargo build --release`, no
  `cargo install`, no extra toolchains or targets unless your task says so.
- Remove a finished worktree with `git worktree remove <path>`.

## Architecture (owner decisions — do not bend)

- One small agent core (`p1-core`): loop + API. It depends only on `p1-contracts`.
  It never names a provider, tool, file format, prompt template or UI.
- Every tool is its own module (crate). Providers only translate wire behaviour and
  contain no tools. Tools contain no provider wire formats and no UI types.
- An agent sees only the prompt and tools assembled for it; an unassembled tool
  cannot be dispatched.
- Delegation is an optional tool module. No mandatory coordinating agent.
- Compile-time composition with ordinary constructors. No plugin loader, service
  locator, global registry or DI framework.
- Public async interfaces are `Send`-capable. One owner per agent's mutable state.
- Unknown usage/cost is `None`, never zero.
- Rust 2024, `unsafe` forbidden, errors via `thiserror` in libraries. Clear
  descriptive names — no mythology names.

## Style

Comments explain why, not what. No speculative abstractions or unused generality.
