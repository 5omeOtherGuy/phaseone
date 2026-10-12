# The owner's workstation

The machine-specific parts of the project rules, moved out of `AGENTS.md` with the owner pack
(#853). `AGENTS.md` holds the rules; this file holds where they land on the workstation.

## Checkouts

- The main checkout is `~/projects/phaseone`; task worktrees are its siblings
  (`../phaseone-<issue>-<slug>`, made by `scripts/new-worktree.sh`).
- `~/projects/iris-agent` and `~/projects/iris-agent-clean` are the read-only donors.

## Build targets

- Every checkout's target lives under `~/.cache/cargo-target/` on the SSD (ext4), and
  `scripts/rustc-serial` is the machine-wide wrapper that serializes rustc.
- `scripts/new-worktree.sh` seeds a new target from `~/.cache/cargo-target/p1-template`, the
  dependencies of a warm target without p1's own crates: on the SSD the compiled outputs are
  hardlinked and cargo's metadata copied; on the HDD build root it is a full 1.7 GiB copy.
  `scripts/retire-worktree.sh` keeps the template current: before it deletes a target it adds
  the target's dependency units the template lacks and drops those of an older compiler
  (`scripts/target-template.sh --absorb <target>`). `scripts/target-template.sh [warm-target]`
  rebuilds the template from one warm target (default: the main checkout's).
- A cold build (new target, no template) grows the SSD by 13 to 17 GiB: start one only with at
  least 25 GiB free (`df -h ~`).

## Selecting this pack

A p1 installed from a release carries the pack in its share and anchors its accounts
(`release.yml` packs `packs/owner`), so it needs nothing. A debug build of a checkout
(`cargo run`, `target/debug/p1`) reads only the source tree's core catalog and connectors;
`~/.config/p1/settings.toml` names the pack so it layers the owner's environments too:

```toml
packs = ["~/projects/phaseone/packs/owner"]
```

A local install anchors the pack's accounts with `scripts/install.sh --local --catalog-root .
--catalog-root packs/owner` (ADR-0196). A build that does not anchor them sends an owner
account's key only to an origin its store entry approved: an entry without one asks for
`p1 login <account> --trust-endpoint` once.
