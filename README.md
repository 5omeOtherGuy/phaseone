# Phaseone (p1)

p1 is a modular coding-agent harness for the terminal. Pick an environment — a route, a
model, the tools and the whole prompt — give it a task, and the agent works in your
workspace.

```sh
p1 --env claude --workspace <repo> --yes "<task>"   # headless
p1 acp                                              # for editors and other ACP clients
```

## Architecture

- **A small core.** `p1-core` is the agent loop and nothing else; it depends only on the
  ports in `p1-contracts`.
- **Ports and adapters.** Providers, tools, policies, the journal, workers, workflows and
  front ends are adapters behind those ports. Tools, providers and policies are
  WebAssembly components under `modules/`.
- **One composition root.** `p1-host` loads the modules an environment names, verified
  against the release manifest. No registry, no auto-registration, no native fallback.
- **Agents see only what is assembled for them.** An unassembled tool cannot run.

Where a part will vary, it gets its own port from the start; `docs/design/seams.md` names
the port for each capability. The rules: ADR-0153.

## Core, connectors, packs

The repository is splitting into three tiers (ADR-0200, epic #840, in progress):

- **Core** — the bare product: crates, modules, a neutral `default` environment and
  API-key routes.
- **Subscription connectors** (`connectors/subscriptions/`) — Claude Code, Codex,
  OpenCode, Cline, GLM and Kimi subscription routes, behind the cargo feature
  `subscriptions`.
- **Owner pack** (`packs/owner/`) — one user's environments, accounts and prompts,
  layered over the core catalog through the same files any other pack would use.

Until the split lands, the root catalog still holds the subscription routes and the owner's
environments.

## Install

Every green commit on `main` is published as a GitHub Release. No Rust toolchain needed.

```sh
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/phaseone/main/scripts/install.sh -o p1-install.sh
bash p1-install.sh            # latest release into ~/.local
p1-update                     # later
```

Logins live in `~/.config/p1/auth.json`: `p1 login <route>` reads an API key from stdin,
`p1 login --list` shows every route. Details: `docs/design/credentials.md`.

## Develop

```sh
scripts/new-worktree.sh <issue>-<slug>   # one worktree per task
cargo test -p <crate> <name>
scripts/pre-push.sh                      # before every push
```

Main takes changes only through a pull request with a green `gate` check, which runs in CI.
How to work here: `AGENTS.md`. Decisions: `docs/adr/`. Design: `docs/design/`.

MIT licence.
