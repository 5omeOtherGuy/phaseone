---
adr: 200
title: phaseone's core is the bare product; subscription connectors and the owner pack are optional parts
status: superseded
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: [217]
sources: [https://github.com/5omeOtherGuy/phaseone/issues/840]
---
# ADR-0200: phaseone's core is the bare product; subscription connectors and the owner pack are optional parts

## Context

The repository mixes the product with the owner's own setup. Owner, 2026-10-11: "Currently, the
phaseone repo mixes foundational work, like the ACP with my own modules and how I would like
phaseone to work for me (e.g. my system prompts, modes, etc.) We need to figure out how to split
those, so phaseone is only the bare bones product and everything we build around it becomes
modules that are not necessarily developed within the same repo." Later the same day: "I am
thinking more of a mono repo, even the subscription connectores should not be part of the core
product", and the goal "that we can do parallel work on different things so my own setup doesn't
hold PRs that are foundational."

What ties the product to the owner's setup today (evidence in epic #840 and the handoff it links):
`crates/p1-host/build.rs` compiles `routes/` and `accounts/` into the credential trust anchor
(ADR-0110, ADR-0139 §2), so an account edit is a Rust change; about sixty test files read the
shipped catalog, so an owner-config edit can fail core tests; `DEFAULT_ENV` is the owner's
`claude` environment; `usage.rs`, `p1-usage/src/probe.rs` and `p1-workflow/src/api.rs` name the
owner's account ids and models; every shipped environment runs on one of the owner's
subscriptions; the owner's process scripts run in the gate. The providers are WebAssembly modules
whose only dialects are the subscription ones (`claude-code-subscription`, `codex-subscription`,
the OpenCode client identity): a core without the connectors has no usable provider and no
pay-as-you-go route. The credential kinds `claude-code-oauth` and `codex-oauth` are a closed
enum spread over `p1-auth`, `p1-host`, `p1-usage` and `build.rs`; no seam lets a separate crate
add a kind or a dialect.

## Decision

One repository with three tiers in separate directories, each usable without the ones below it:

1. **Core**, the bare product: `crates/`, `modules/`, the catalog at the repository root
   (`environments/`, `routes/`, `accounts/`, `profiles/`, `modules.lock`) holding one neutral
   `default` environment and API-key routes (`anthropic`, `openai`, `openai-chat`; ADR-0197),
   the product scripts and docs. The core builds and passes its tests with no other tier
   present. The default environment comes from `settings.toml default_environment`, else
   `default`.
2. **Subscription connectors**, `connectors/subscriptions/`: the data and tests of the routes
   that reach a subscription (Claude Code OAuth, Codex OAuth, OpenCode Zen and Go, Cline,
   GLM and Kimi coding plans), with inline credentials so the tier needs no owner account.
   Their code stays in the core crates (`p1-auth`, `p1-usage`, the three provider crates and
   modules, `p1-host`) behind one cargo feature, `subscriptions`, on by default; a route that
   names a gated credential kind or dialect in a build without the feature fails naming the
   connectors. Extracting that code into crates of its own needs a credential-method port and a
   provider dialect port first and is a later decision record.
3. **Owner pack**, `packs/owner/`: the owner's environments, accounts, prompt fragments and
   profiles, `subagents.toml`, the owner's documents and process scripts, and the pack's own
   checks. A pack layers between the user's configuration directory and the shipped catalog
   (ADR-0201) and is never read implicitly.

Two builds are proven in CI inside the existing gate workflow: the **bare** build (core only,
`subscriptions`, `shadow-hook` and the frozen TUI off) runs the core tests, `p1 env check` over
the core catalog and an ACP session with `default`; the **distribution** build (the owner's
install) composes core, connectors and the owner pack, anchors all three (ADR-0196) and packs
exactly those roots. A pull request that changes only `packs/**` runs no Rust job, only
`p1 env check` and the pack's checks against the latest released p1.

Boundary rule, independent of the layout: an optional tier uses only interfaces another
repository could also use (route, account, environment and profile files, `settings.toml`,
`P1_PACKS`, the module manifest); nothing in the core names a connector's or the pack's ids.
`packs/owner` can move to its own repository later with `git subtree split`; the owner chose
one repository because most setup changes are coupled to product changes today and a second
repository would turn each into a core release followed by a pack change.

## Consequences

- Core pull requests and pack pull requests touch disjoint files and merge independently; a
  pack-only change waits for no Rust build.
- The core gains what it lacked: API-key dialects, pay-as-you-go routes, a neutral default
  environment, a `p1 env check` command (ADR-0198), pack layering (ADR-0201) and a trust anchor
  built from the build's catalog roots (ADR-0196). `usage.rs` labels, probe shapes and workflow
  role defaults come from catalog data.
- Owner-content tests (prompt coherence, subagent environments, shipped model rows) leave the
  Rust test suite and become pack checks run against the released binary; the connector-content
  tests move with the connector data into a feature-gated test crate.
- The owner's live logins keep working: connector routes keep their `store_id`s, and the
  distribution build anchors the owner pack's accounts.
- A change that needs both a core part and a pack part stays one pull request, which runs the
  full gate. A pack in another repository would not have that.
- The connector code is gated, not extracted: `cfg(feature)` blocks in six crates until the
  ports exist. The `p1/` module namespace rule (ADR-0087) is unchanged; truly external modules
  need their own record.

## Alternatives considered

- Owner pack in its own repository now, core and connectors in phaseone (the lead's proposal of
  2026-10-11): rejected for now because the pack's changes are coupled to product changes and
  the pack-only CI path gives the same independence; kept as a later option.
- Extract the connectors into `connectors/` crates and modules immediately: rejected for this
  slice because it needs two new ports and a module-side seam first (weeks of work) while the
  feature gate makes the bare product real now.
- Separate repositories for every module (the owner's first wording, "not necessarily developed
  within the same repo"): superseded by the owner's later words; the boundary rule keeps it
  possible.

## Evidence

Research of 2026-10-11 (epic #840; handoff `handoff-repo-split.md` of the p1 lead): provider
modules `modules/p1-module-provider-*` build the provider crates with `default-features =
false`; `crates/p1-host/Cargo.toml` lists the provider crates as dev-dependencies only
(ADR-0093); `crates/p1-provider-anthropic/src/lib.rs` `MessagesAccount` and
`crates/p1-provider-openai/src/lib.rs` `ResponsesAccount` hold only subscription dialects;
`crates/p1-auth/src/spec.rs` `CredentialKind` is closed and matched in `resolve.rs`,
`crates/p1-host/build.rs`, `login.rs`, `routes.rs` and `crates/p1-usage/src/probe.rs`;
`crates/p1-host/src/usage.rs` and `probe.rs` name sixteen owner ids. Verification of the split
is the epic's end-to-end recipe: bare tests green with `p1 env show default` and an ACP session;
distribution install with `p1 models` reporting no errors for the owner pack and `p1 env show
claude2` resolving the borrowed login; `scripts/test_stage_release.py` and
`scripts/test_install.py` green.
