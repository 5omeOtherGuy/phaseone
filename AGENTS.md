# p1 — instructions for agents and humans

p1 is a lean, modular Rust coding harness. Direction: `docs/design/` (start at
`README.md`). Settled choices: `DECISIONS.md`. Work items: GitHub Issues.

Several agents work in this repository at the same time. Everything below exists to
make that fast and conflict-free.

## Swarm protocol

- **One worktree per task.** Never run two agents in one checkout.
  `scripts/new-worktree.sh <task-slug>` creates `../phaseone-<task-slug>` on a fresh
  `task/<task-slug>` branch. Each worktree has its own `target/`, so builds never
  collide and no shared `CARGO_TARGET_DIR` is ever needed.
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

## Gate

`scripts/gate.sh` is the only required check: `cargo fmt --check`,
`clippy -D warnings`, all tests, core isolation. CI runs exactly the same script.
It must be green before a merge; it is not required for every intermediate commit.

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

- 7 GB laptop: `CARGO_BUILD_JOBS=2` (the gate sets it), one build at a time per
  worktree. Prefer focused runs: `cargo test -p <crate> <filter>`.

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
