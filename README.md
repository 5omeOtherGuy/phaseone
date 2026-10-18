# Phaseone (p1)

A lean, modular Rust coding harness that adapts itself to the model it runs.

Status: pre-release. You can try it, but nothing is promised yet: commands, environments, packs and the release format may change between releases.

[![gate](https://github.com/5omeOtherGuy/phaseone/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/5omeOtherGuy/phaseone/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/5omeOtherGuy/phaseone)](https://github.com/5omeOtherGuy/phaseone/releases/latest)
[![licence](https://img.shields.io/github/license/5omeOtherGuy/phaseone)](LICENSE)

## What it does

p1 is a coding-agent harness for the terminal. Pick a model, give it a task, and the agent
works in your workspace. The model is the selector: p1 derives the wire that reaches the
model (a connector) and the way of working that suits its family (a kit), and an environment
or a pack can override any part of that, the whole prompt included.

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

The repository holds:

- **`crates/`**: the native code: the contracts, the core, the host (the one composition
  root), the module runtime, the adapters and the front ends.
- **`modules/`**: p1's own WebAssembly modules (tools, policies, providers, context), the
  module SDK and the WIT worlds.
- **`connectors/<vendor>/`**: one connector per vendor, the wire: routes, the login and
  usage-probe modules, the profiles only it serves. `connectors/README.md` lists them.
- **`kits/<family>/`**: one kit per model family, the way of working: tools, prompt profile,
  compaction, concurrency, options, with per-model overrides over `kits/base`.
  `kits/README.md` lists them.
- **`environments/`, `profiles/`**: the shipped environment (`default`, a model plus
  overrides) and the model profiles. `catalog-roots.txt` lists the shipped catalog roots: `.`
  and each connector.
- **`packs/owner/`**: one user's environments, accounts and prompts, installed as a pack and
  never read implicitly.

## Install

A GitHub Release is published when the owner dispatches `release.yml`: it builds the newest
commit that passed the gate on `main` (ADR-0218). No Rust toolchain needed. A release
publishes one composition (ADR-0217): the shipped catalog with every connector.

```sh
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/phaseone/main/scripts/install.sh -o p1-install.sh
bash p1-install.sh            # the latest release into ~/.local
p1-update                     # later
```

## Usage

```sh
p1 login anthropic < key.txt                     # store an API key; or set ANTHROPIC_API_KEY
p1 --workspace <repo> "<task>"                   # headless run on the default environment
p1 --model <model> --workspace <repo> "<task>"   # any model `p1 models` lists
p1 acp                                           # for editors and other ACP clients
```

`p1 --help` lists every command; `p1 login --list` shows every route and where its credential
comes from.

## Configuration

- Logins live in `~/.config/p1/auth.json`; `p1 login <route>` reads an API key from stdin.
  Details: `docs/design/credentials.md`.
- `~/.config/p1/settings.toml` holds the default model, the enabled models and the preferred
  routes (`docs/design/model-selection.md`), and `packs`, the extra catalog roots to layer over the
  shipped one (or `P1_PACKS`, ADR-0201).
- `p1 env show <name>` prints where each part of an environment comes from; `p1 env check`
  validates every environment without credentials or network.

## Compatibility

- Release binary: Linux x86_64 with glibc 2.39 or newer; it is built on Ubuntu 24.04. Checked
  2026-10-18 with release `main-ca4a85651478`: Debian 12 (glibc 2.36) refuses it.
- Building from source: Rust 1.98.1 with the `wasm32-unknown-unknown` target and the
  wasm-tools version pinned in `modules/toolchain.pins`; `scripts/module-toolchain.sh --check`
  verifies both.

## Development

```sh
scripts/new-worktree.sh <issue>-<slug>   # one worktree per task
cargo test -p <crate> <name>             # a focused test
scripts/pre-push.sh                      # before every push
```

The gate is `scripts/gate.sh`. It runs in CI on GitHub's runners as the required `gate` check,
and `main` takes changes only through a pull request with that check green. How to work here:
`AGENTS.md`. Decisions: `docs/adr/`. Design: `docs/design/`.

## Contributing, support and security

- [CONTRIBUTING.md](CONTRIBUTING.md): how a change gets in.
- Questions and bugs: [issues](https://github.com/5omeOtherGuy/phaseone/issues).
- Vulnerabilities: report them privately, as [SECURITY.md](SECURITY.md) describes.
- [Code of Conduct](CODE_OF_CONDUCT.md).

## Licence

MIT, see [LICENSE](LICENSE).
