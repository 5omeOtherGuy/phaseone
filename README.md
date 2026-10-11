# Phaseone (p1)

p1 is a coding-agent harness for the terminal. You give it a prompt and an
environment — a directory that names a route, a model profile, the tools and the whole
prompt — and the agent works in your workspace (`environments/`, `p1 --help`).
Modularity is what p1 is for: a small core runs the agent loop, and everything else is
a module behind a port (ADR-0153).

## Architecture

Four rules shape every part of p1 (ADR-0153):

- **Microkernel.** `p1-core` is the agent loop and its API only, and depends only on
  `p1-contracts` (ADR-0002, enforced by `scripts/check-core-isolation.sh`). Providers,
  tools, context and authorization policies, the journal, workers, workflows and front
  ends are modules around it.
- **Ports and adapters.** A port is a contract in `crates/p1-contracts` (`Provider`,
  `Tool`, `ContextPolicy`, `AuthorizationPolicy`, `FrontEndPort`, …) or a WIT interface
  under `modules/wit/`. An adapter is a crate that implements it, natively or as a
  WebAssembly component under `modules/`.
- **Protected Variations.** A part known to vary — a source, provider, tool, front end
  or file format — gets its own contract and module in the first slice that touches it,
  even with one implementation. The seam catalog `docs/design/seams.md` names the port
  for each capability.
- **dsh is the floor.** Every seam p1 builds is at least as modular as its equivalent in
  the DeepSeek harness (dsh). A missing feature is not below the floor; a weaker seam is.

```diagram
  ACP client ── p1 acp ──┐        ┌── p1 "<task>" (headless)
                         ▼        ▼
              ┌──────────────────────────────┐
              │ p1-host: composition root    │
              │ reads the environment, loads │
              │ modules by name              │
              └───────┬──────────────┬───────┘
                      │ wires        │ runs
                      ▼              ▼
  ┌───────────────────────────┐   ┌──────────────┐
  │ adapters                  │   │ p1-core      │
  │ providers, tools,         │──▶│ agent loop   │
  │ policies, journal,        │   └──────┬───────┘
  │ workers, workflows, MCP   │          │
  └─────────────┬─────────────┘          │
                │ implement              │ calls
                ▼                        ▼
       ┌────────────────────────────────────────┐
       │ p1-contracts: the ports                │
       └────────────────────────────────────────┘
```

Composition is explicit at that one root. `p1-host` loads tools, providers and policies
as WebAssembly components by name from the environment file, each verified against the
release's module manifest before it compiles, with no native fallback; there is no
service locator, global registry, auto-registration or DI framework (ADR-0071,
ADR-0103). Native by design: the core, the journal, the worker service, the workflow
interpreter, the provider transport and the sandbox boundary (ADR-0081, ADR-0086).

- **An agent sees only what is assembled for it**: its environment's prompt, the tools
  the environment lists and the provider behaviour for its model. An unassembled tool
  cannot dispatch, and delegation and workflow tools are explicit grants (ADR-0162).
- **Providers only translate wire behaviour** and hold no tools; tools hold no provider
  wire formats and no UI types.
- **Delegation is optional.** A worker tool hands a bounded task to another agent on
  another route; there is no mandatory coordinating agent.
- **Unknown usage or cost is `None`, never zero.**

`p1 acp` is the single interactive door: p1 speaks the agent side of ACP over stdio, and
every interactive screen is an ACP client attached through the ACP-neutral front-end
port (ADR-0152, ADR-0154). The line front end is the headless driver. The terminal UI
(`p1 --tui`, `crates/p1-tui`) is frozen as a parts donor behind the default-on `tui`
feature; p1's next front end is still to be chosen.

## Core, connectors and packs

ADR-0200 makes the core the bare product and splits the repository into three tiers,
each usable without the ones below it:

| Tier | Where | What |
|---|---|---|
| Core | `crates/`, `modules/`, the root catalog (`environments/`, `routes/`, `accounts/`, `profiles/`, `modules.lock`) | The product: a neutral `default` environment and pay-as-you-go API-key routes (`anthropic`, `openai`, `openai-chat`) |
| Subscription connectors | `connectors/subscriptions/` | Data and tests of the subscription routes (Claude Code and Codex OAuth, OpenCode Zen and Go, Cline, GLM and Kimi coding plans); their code stays in the core crates behind the cargo feature `subscriptions`, on by default |
| Owner pack | `packs/owner/` | The owner's environments, accounts, prompts, profiles, documents and process scripts; a pack layers between the user's configuration directory and the shipped catalog and is never read implicitly |

The boundary rule: an optional tier uses only interfaces another repository could use
too — route, account, environment and profile files, `settings.toml`, `P1_PACKS`, the
module manifest — and nothing in the core names a connector's or the pack's ids. CI
proves two builds: the bare build (core only) and the distribution build (core,
connectors and the owner pack), which is what the release installs. A pull request that
changes only `packs/**` runs no Rust job, so the owner's setup never holds up a core
change.

## Use cases

- Run a coding task headlessly: `p1 --env claude --workspace <repo> --yes "<task>"`
  (docs/SLICE-REPORT.md).
- Drive p1 from an editor or any other ACP client: `p1 acp` (ADR-0154).
- Resume a session from its JSONL journal, compacting once first if needed:
  `p1 --session s.jsonl --resume --compact "<prompt>"` (docs/design/journal.md).
- Hand a bounded sub-task to a worker on another route mid-run, from an environment
  that grants the worker tools (docs/design/delegation.md; ADR-0162).
- Orchestrate workers with a Rhai workflow whose steps can each run in their own
  git worktree: `p1 workflow run FILE --arg K=V --yes` (docs/design/workflows.md;
  ADR-0073).
- List and verify the module set of an installed release against its release
  manifest: `p1 modules list`, `p1 modules verify` (ADR-0079).
- Select which official package a module name resolves to with a `modules.lock`
  next to an environments directory, e.g. `read` to the `p1/read` package
  (crates/p1-assembly/src/modules_lock.rs).

## Status (2026-10-11)

Pre-alpha. The first slice ran real coding tasks on the Claude and Codex subscription
routes with session resume and delegation; the measured record is docs/SLICE-REPORT.md
(2026-09-20). Tools, providers and policies load as WebAssembly components, and the
gate's shipping audit (`scripts/check-module-boundaries.sh --shipping`) fails on any
native fallback.

The core/connectors/owner-pack split (ADR-0200, epic #840) is in progress, so the root
catalog still holds the owner's environments, subscription routes and accounts, `claude`
is the default environment, and `connectors/` and `packs/` do not exist yet. Landed: the
`tui` feature (#849), catalog fixtures inlined in the tests (#844), account labels and
usage probes read from catalog data (#848). In review: API-key dialects, the core routes
and the `default` environment (#847), pack layering and settings (#845), `p1 env check`
(#846), a trust anchor built from the selected catalog roots (#850), host tests off the
shipped catalog (#842). After those: the `subscriptions` feature with the bare CI build,
moving the connector data and the owner pack out of the root catalog, the pack-only CI
path and release composition (#851 to #855).

## Where to read next

- docs/adr/0153-*.md — the architecture rules; docs/design/seams.md — the seam catalog.
- docs/adr/0200-*.md — the core, connectors and owner pack split.
- docs/design/README.md — the design notes and the module-boundary documents.
- docs/SLICE-REPORT.md — what the first slice does, measured, and what is weak.
- `scripts/adr.py list` — the index of settled decisions (generated, ADR-0191).

## Install and update

p1 is published as a GitHub Release for every commit whose gate is green on `main`: the
binary `p1-linux-x86_64`, the shipped data `p1-share.tar.gz` (`environments/`, `routes/`,
`profiles/`), and a sha256 file for each. Installing needs no Rust toolchain and no
compile on your machine (ADR-0065). It needs `curl` (or `gh`), `sha256sum`, and python3 —
3.12, or 3.8.17 / 3.9.17 / 3.10.12 / 3.11.4 with the security backports — whose `tarfile`
data filter validates and extracts the share archive; a missing or older interpreter is
refused by name, before anything is installed.

```sh
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/phaseone/main/scripts/install.sh -o p1-install.sh
bash p1-install.sh                 # --latest into ~/.local
bash p1-install.sh --prefix /opt/p1
bash p1-install.sh --from-release main-1a2b3c4d5e6f   # a specific published commit
bash p1-install.sh --from-release main-1a2b3c4d5e6f --force  # reinstall the same release
```

The release install is the normal path: `scripts/install.sh --latest` (also the default when
the downloaded `p1-install.sh` is run without a mode) installs the GitHub Release built by
CI, so no Rust toolchain is needed here. A local build is only the fallback, and needs this
checkout:

```sh
scripts/local-cargo-config.sh
CARGO_TARGET_DIR="$HOME/.cache/cargo-target/p1-local" scripts/install.sh --local
```

The installer requires an absolute, distinct per-task target below
`~/.cache/cargo-target` on the SSD. It refuses a relative or non-ext4 target, runs Cargo with
`CARGO_BUILD_JOBS=3`, and sets `RUSTC_WRAPPER` to the checkout's `scripts/rustc-serial`, which
admits at most three concurrent `rustc` processes. It admits a local build only with at least
12 GiB free on the SSD; preserve the 8 GiB SSD floor. The internal HDD is retired for builds.

The installer

- stages the binary, updater and share together and swaps them only after validation, so a
  failed install leaves the previous binary and data together;
- writes `<prefix>/bin/p1` (0755) and the data at
  `<prefix>/share/p1/{environments,routes,profiles,accounts}`;
- copies itself to `<prefix>/share/p1/install.sh` and installs `<prefix>/bin/p1-update`, so
  updates need no checkout;
- never reads, writes or deletes anything under `${XDG_CONFIG_HOME:-$HOME/.config}/p1` —
  your logins and local overrides stay yours, and a prefix whose `bin` or `share` resolves
  into that tree is refused.

The archive rejects duplicate or colliding entries and bounds extracted size and member
count before extraction. An explicit `main-<sha>` install requires the module manifest's
tag and commit to match. An already installed module set must pass the runtime's integrity
check before a matching tag can be treated as a no-op. Installer provenance records
whether modules were required, independently of the module directory itself; missing
or unverified provenance forces a reinstall rather than claiming an intact release.

Installing the release already present is a no-op; pass `--force` to reinstall it. An
explicit `--from-release TAG` may install any published tag, including an older one, so
a downgrade is allowed. `p1-update` follows the release marked latest.

Update later with:

```sh
p1-update                          # <prefix>/bin/p1-update, or scripts/update.sh in a checkout
```

`p1-update` runs the installer copied at the last install. An installer from before the
shipped account files (ADR-0139) does not know the archive's `accounts/` root and refuses the
release before writing anything; update once with the two `curl`/`bash` lines above, and
`p1-update` works again from then on.

`p1 --version` prints the package version with the commit it was built from and the build
date, e.g. `p1 0.0.1 (1a2b3c4d5e6f 2026-09-24)`. `<prefix>/bin` must be on your `PATH`;
the installer warns when it is not.

## Logging in

API-key routes take one key from stdin — never from an argument, which would land in
shell history and in `ps`:

```sh
p1 login opencode-go-2-subscription     # reads one key, hidden on a terminal
p1 login --list                         # every route, its credential kind and its source
p1 logout opencode-go-2-subscription    # removes that route's entry (an imported OAuth copy too)
```

The key is written to `~/.config/p1/auth.json` (0600 in a 0700 directory); a store
anyone but the owner can read is refused. Piped input works as well:
`p1 login <route> < keyfile`. A documented environment variable still wins over the
store, and `login` says so when it does.

Every shipped route is self-contained (`store_only`, ADR-0061): p1 reads its own store
and the route's documented environment variable, and never another tool's login file —
no Pi, OpenCode, Claude Code or Codex credentials at runtime. `p1 login --list` marks
such a route `[p1 store only]`. The two OAuth routes need an independent grant in p1's
store; copying a live CLI refresh token is unsafe because it rotates, and p1 does not
ship an OAuth browser flow yet, so `p1 login <oauth-route>` says so instead of pointing
at the CLI (`docs/design/credentials.md` §8).

The second Claude subscription (`claude2/…`, route `anthropic-subscription-2`, ADR-0074)
is the exception: it borrows the Claude Code login in `~/.claude-2` in place (log in there
with `CLAUDE_CONFIG_DIR=~/.claude-2 claude`), after p1's own store. On a machine without
Claude Code, copy a login into p1's store instead — no token is printed:

```sh
p1 login anthropic-subscription-2 --from-claude-code             # the route's login_dir
p1 login anthropic-subscription --from-claude-code ~/.claude     # any Claude Code directory
```

An imported copy wins over the live Claude Code login until `p1 logout <route>` removes
it (`p1 logout anthropic-subscription-2` makes the route borrow `~/.claude-2` again). The
copied refresh token rotates: once p1 or Claude Code refreshes it, the other copy is
stale, so import where only p1 uses that login (`docs/design/credentials.md` §10).

## Layout

| Path | What |
|---|---|
| `crates/p1-core` | The agent loop and its API; depends only on `p1-contracts` |
| `crates/p1-contracts` | The ports: contracts between the core, the host and the adapters |
| `crates/p1-host` | The `p1` binary and composition root |
| `crates/p1-*` | One crate per adapter or boundary: providers, tools, assembly, journal, workers, workflows, ACP, MCP client, module runtime |
| `modules/` | WebAssembly component packages and the WIT interfaces (`modules/wit/`) |
| `environments/`, `routes/`, `accounts/`, `profiles/`, `modules.lock` | The shipped catalog |
| `docs/design/` | Design notes; `seams.md` is the seam catalog |
| `docs/adr/` | Architecture Decision Records: the settled decisions and their evidence |
| `AGENTS.md` | How humans and agents work in this repo |
| `scripts/gate.sh` | The required `gate` check, run in CI: fmt, clippy `-D warnings`, tests, core isolation, shipping audit |
| `scripts/install.sh` | Install or update p1 from the release channel (`--latest`, `--from-release`, `--local`) |

## Build

```sh
scripts/local-cargo-config.sh   # a target of this checkout's own and the rustc-serial wrapper
cargo build
cargo test -p <crate> <name>    # focused tests of what you changed
scripts/pre-push.sh             # fmt, clippy, script tests; run once before a push
```

Linux/macOS, Rust stable (2024 edition). The full gate runs on GitHub's runners for
every pull request; main takes changes only through a pull request with a green `gate`.

Release builds are CI's: `.github/workflows/release.yml` builds in release profile on a
GitHub runner after a green `main` and publishes the release that `install.sh` consumes.
The one local exception is the documented `--local` fallback. Do not run another
`cargo build --release` or `cargo install` on a workstation (AGENTS.md).

## Working here

The repo is built for parallel agents: one git worktree per task
(`scripts/new-worktree.sh`), one small issue per task, short-lived branches, trunk-based
merges through pull requests. See `AGENTS.md` and open an issue to claim work.

## Licence

MIT — see `LICENSE`.
