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

## Layout

One repository (ADR-0217, epic #924, in progress). The model is the selector: a user picks a
model, and p1 derives the wire that reaches it and the way of working that suits it. Both are
packs (ADR-0201) a user can install, copy and tune.

- **`crates/`** — the native code: the contracts, the core, the host (the one composition
  root), the module runtime, the adapters and the front ends.
- **`modules/`** — p1's own WebAssembly modules (tools, policies, providers, context), the
  module SDK and the WIT worlds.
- **`connectors/<vendor>/`** — one connector per vendor, the wire: routes, the login and
  usage-probe modules, the profiles only it serves. `connectors/README.md` lists them.
- **`kits/<family>/`** — one kit per model family, the way of working: tools, prompt profile,
  compaction, concurrency, options, with per-model overrides over `kits/base`.
  `kits/README.md` lists them.
- **`environments/`, `profiles/`** — the shipped environments (thin: a model plus overrides)
  and the model profiles; `routes/` dissolves into the connectors.
- **`packs/owner/`** — one user's environments, accounts and prompts, installed as a pack and
  never read implicitly.

Until the units of #924 land, the routes live in `routes/` and in the connector release
`connectors.lock` pins, and the environments stay complete orders.

## Install

Once a day, the newest green commit on `main` is published as a GitHub Release, and the newest
30 are kept (ADR-0216). No Rust toolchain needed.
A release carries two compositions of the same commit (ADR-0207): the distribution (core,
subscription connectors and owner pack) and the bare product (the core alone).

```sh
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/phaseone/main/scripts/install.sh -o p1-install.sh
bash p1-install.sh            # the latest distribution into ~/.local
bash p1-install.sh --bare     # or the bare product
p1-update                     # later; keeps the composition you installed
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
