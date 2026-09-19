# Phaseone (p1)

A lean, modular Rust coding harness that adapts itself to the model it runs.

You type a prompt and the agent works. One small agent core (loop + API); everything
else — providers, tools, sessions, frontends, delegation — is a module around it,
composed with ordinary constructors.

**Status: pre-alpha.** The workspace, the gate and the design baseline exist. The
first usable slice (`docs/design/seams.md` §10) is being built.

## What makes it different

- **The harness reshapes itself around the model.** An agent gets the prompt, tool
  descriptions and provider behaviour suited to its model — and sees only those. An
  unassembled tool cannot be dispatched.
- **Tools are always their own modules.** Providers only translate wire behaviour and
  contain no tools; tools contain no provider wire formats and no UI types.
- **Delegation is optional.** A tool module lets an agent hand a bounded task to
  another agent on another model and be woken when it finishes. There is no mandatory
  coordinating agent, and the harness is a plain coding agent without it.
- **Efficiency across the whole job.** Low memory and startup cost, lean contexts,
  few dependencies, unknown token/cost reported as `None` — never zero.

## Layout

| Path | What |
|---|---|
| `crates/p1-contracts` | Shared contracts between the core and its modules |
| `crates/p1-*` | One crate per module boundary, added increment by increment |
| `docs/design/` | Design baseline: pillars, one-page design, seams + acceptance |
| `DECISIONS.md` | Why things are the way they are |
| `AGENTS.md` | How humans and agents work in this repo |
| `scripts/gate.sh` | The only required check: fmt, clippy `-D warnings`, tests, core isolation |

## Build

```sh
cargo build
scripts/gate.sh     # must be green before anything merges into main
```

Linux/macOS, Rust stable (2024 edition). On 7 GB-class machines builds default to
`CARGO_BUILD_JOBS=2`.

## Working here

The repo is built for parallel agents: one git worktree per task, one small issue per
task, short-lived branches, trunk-based merges. No review gate, no branch protection.
See `AGENTS.md` and open an issue to claim work.

## Licence

MIT — see `LICENSE`.
