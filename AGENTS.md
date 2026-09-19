# Phaseone (p1) — repository rules for agents

p1 is a lean, modular Rust coding harness that adapts itself to the model it runs.
Design baseline: `docs/design/` (start at `docs/design/README.md`). Decisions:
`DECISIONS.md`. Current state: `STATUS.md`.

## Hard rules
- No sudo. No package installs. No commits, pushes, branch switches, `git stash`,
  `git add -A`, rebases or git config changes — the lead commits by explicit path.
- Work only inside the workspace/worktree named in your brief and its owned paths.
- Never read, print, log, store or put into fixtures any credential, token,
  Authorization header, private prompt or raw authenticated traffic.
- Never modify `/home/phaseonebig/projects/iris-agent` or `iris-agent-clean`
  (read-only donors). Copy code into p1 and adapt it; note the donor path in your handoff.
- No live network in unit/conformance tests. No real user data directories; use
  `tempfile`/scratch dirs. No sleep-based timing assertions — use fake time or
  explicit synchronisation.
- Never delete, weaken, skip or bend a frozen acceptance test or its fixtures. If a
  check contradicts the spec, leave it failing and explain.

## Build
- 7 GB laptop: `CARGO_BUILD_JOBS=2`, one build at a time per worktree, never a shared
  `CARGO_TARGET_DIR`. Prefer focused runs: `cargo test -p <crate> <filter>`.
- Gate (must be green for a finished increment): `scripts/gate.sh`
  (fmt check, clippy `-D warnings`, all tests, core isolation).

## Architecture rules (owner decisions — do not bend)
- One small agent core (`p1-core`): loop + API. It depends only on `p1-contracts`.
  It never names a provider, tool, file format, prompt template or UI.
- Every tool is its own module (crate). Providers only translate wire behaviour and
  contain no tools. Tools contain no provider wire formats and no terminal/UI types.
- An agent sees only the prompt and tools assembled for it; an unassembled tool
  cannot be dispatched.
- Delegation is an optional tool module. No mandatory coordinating agent.
- Compile-time composition with ordinary constructors. No plugin loader, service
  locator, global registry or DI framework.
- Public async interfaces are `Send`-capable. One owner per agent's mutable state.
- Unknown usage/cost is `None`, never zero.
- Clear descriptive names. No mythology names (no Nexus, Mimir, Wayland…).

## Style
- Rust 2024, `unsafe` forbidden, errors via `thiserror` in libraries. Keep dependencies
  few; ask in the handoff before adding one that is not already in the workspace.
- Comments explain why, not what. No speculative abstractions or unused generality.
