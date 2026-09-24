# Phaseone (p1)

A lean, modular Rust coding harness that adapts itself to the model it runs.

You type a prompt and the agent works. One small agent core (loop + API); everything
else — providers, tools, sessions, frontends, delegation — is a module around it,
composed with ordinary constructors.

**Status: pre-alpha, first usable slice done (2026-09-20).** `p1 --env claude|gpt "prompt"`
runs a real coding task on the Claude and Codex subscription routes, each with its own
prompt and tools; sessions resume from a JSONL journal; an agent can delegate to a worker
on the other route and is woken when it finishes. What was built, measured and what is
weak: `docs/SLICE-REPORT.md`.

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
bash p1-install.sh --local         # build this checkout (needs CARGO_TARGET_DIR or /mnt/build)
```

The installer

- stages the binary, updater and share together and swaps them only after validation, so a
  failed install leaves the previous binary and data together;
- writes `<prefix>/bin/p1` (0755) and the data at `<prefix>/share/p1/{environments,routes,profiles}`;
- copies itself to `<prefix>/share/p1/install.sh` and installs `<prefix>/bin/p1-update`, so
  updates need no checkout;
- never reads, writes or deletes anything under `${XDG_CONFIG_HOME:-$HOME/.config}/p1` —
  your logins and local overrides stay yours, and a prefix whose `bin` or `share` resolves
  into that tree is refused.

Installing the release already present is a no-op; pass `--force` to reinstall it. An
explicit `--from-release TAG` may install any published tag, including an older one, so
a downgrade is allowed. `p1-update` follows the release marked latest.

Update later with:

```sh
p1-update                          # <prefix>/bin/p1-update, or scripts/update.sh in a checkout
```

`p1 --version` prints the package version with the commit it was built from and the build
date, e.g. `p1 0.0.1 (1a2b3c4d5e6f 2026-09-24)`. `<prefix>/bin` must be on your `PATH`;
the installer warns when it is not.

## Logging in

API-key routes take one key from stdin — never from an argument, which would land in
shell history and in `ps`:

```sh
p1 login opencode-go-2-subscription     # reads one key, hidden on a terminal
p1 login --list                         # every route, its credential kind and its source
p1 logout opencode-go-2-subscription    # removes that route's entry
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
| `crates/p1-*` | One crate per module boundary: core, providers, tools, assembly, journal, workers, host |
| `environments/` | Per-model environment files: route, tools, whole prompt |
| `docs/design/` | Design baseline: pillars, one-page design, seams + acceptance |
| `docs/adr/` | Architecture Decision Records: the settled decisions and their evidence |
| `DECISIONS.md` | Why things are the way they are |
| `AGENTS.md` | How humans and agents work in this repo |
| `scripts/gate.sh` | The only required check: fmt, clippy `-D warnings`, tests, core isolation |
| `scripts/install.sh` | Install or update p1 from the release channel (`--latest`, `--from-release`, `--local`) |

## Build

```sh
cargo build
scripts/gate.sh     # must be green before anything merges into main
```

Linux/macOS, Rust stable (2024 edition). On 7 GB-class machines builds default to
`CARGO_BUILD_JOBS=2`.

Release builds are CI's alone: `.github/workflows/release.yml` builds in release profile on
a GitHub runner after a green `main` and publishes the release that `install.sh` consumes.
Do not run `cargo build --release` or `cargo install` on a workstation (AGENTS.md).

## Working here

The repo is built for parallel agents: one git worktree per task, one small issue per
task, short-lived branches, trunk-based merges. No review gate, no branch protection.
See `AGENTS.md` and open an issue to claim work.

## Licence

MIT — see `LICENSE`.
