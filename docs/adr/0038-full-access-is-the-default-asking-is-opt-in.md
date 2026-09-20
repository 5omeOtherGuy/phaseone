---
adr: 38
title: Full access is the default; asking is opt-in
status: accepted
date: 2026-09-20
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/design/assembly.md, crates/p1-host/src/policy.rs, crates/p1-host/src/cli.rs]
---
# ADR-0038: Full access is the default; asking is opt-in

## Context

The host permitted only read-only tool calls in headless runs unless `--yes` was given, and
asked on the terminal in interactive runs. p1 is now used for its own development, mostly
unattended, through worker dispatch. Owner, 2026-09-20: "I want you to make full access /
dangerously-skip-permissions mode the default in p1 so we don't get blockers here. We can't
risk running into problems here during development."

## Decision

Every tool call is permitted by default, headless and interactive. `--ask` opts into the
previous restrictive policy (headless: read-only; interactive: ask per call, with "always for
this tool"). `--yes` stays accepted as a no-op; `--yes --ask` is a usage error. Nothing
changes in the core (ADR-0024: the core only knows Permit/Deny) — this is the host's default
policy object. Workers share the parent's policy as before.

## Consequences

- No run is blocked or silently degraded to read-only because a flag was forgotten.
- The default protects nothing: a model can run any command as the user. The boundaries that
  remain are the file tools' workspace confinement (always on), the shell environment
  allow-list (always on) and the bubblewrap sandbox (opt-in, `--sandbox workspace`), which
  is the recommended companion for unattended runs on a machine that matters.
- Anyone who wants to be asked must say so with `--ask`.

## Alternatives considered

- Keep the restrictive default and pass `--yes` everywhere: one forgotten flag turns an
  unattended job into a read-only no-op — exactly the blocker the owner rules out.
- Make the sandbox the default at the same time: it can itself block legitimate work (git
  metadata of a worktree, tools outside the allow-list); the owner wants no blockers now.

## Evidence

Owner instruction quoted above. Behaviour: `cargo test -p p1-host` (policy and CLI tests).
