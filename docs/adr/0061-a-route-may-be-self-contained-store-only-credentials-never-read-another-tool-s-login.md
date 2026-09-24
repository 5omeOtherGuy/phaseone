---
adr: 61
title: A route may be self-contained: store-only credentials never read another tool's login
status: accepted
date: 2026-09-24
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/design/credentials.md, docs/design/routes-and-profiles.md, crates/p1-auth/src/spec.rs, crates/p1-auth/src/resolve.rs, crates/p1-host/src/login.rs, routes/anthropic-subscription.toml, routes/openai-codex-subscription.toml]
---
# ADR-0061: A route may be self-contained: store-only credentials never read another tool's login

## Context

Owner order, 2026-09-24: every active p1 model route must be self-contained within p1.
In particular, p1 must not silently read Pi, OpenCode, Claude Code or Codex login files
at runtime. The old chain per route (ADR-0040) was: documented environment variable, then
p1's own store, then another tool's login. An `api-key` route could already opt out of the
third step with `borrow = []`, but the two OAuth kinds (`claude-code-oauth`, `codex-oauth`)
ALWAYS appended the CLI's own login file after p1's store, with no way to say "p1's store
only". So a route that looked self-contained still silently fell back to whichever CLI
happened to be logged in.

Two more facts shape this decision:

1. A refresh token ROTATES. Copying an active refresh token out of a working CLI as a
   long-term mechanism is unsafe: the first refresh invalidates it in the source file,
   and the source CLI breaks. A self-contained route therefore needs its OWN OAuth grant,
   not a copied one.
2. p1's store already refreshes the `oauth` entries it holds (write-back under the store
   lock). What is missing is the acquisition step that MINTS a grant; p1 ships no
   independent OAuth flow yet.

## Decision

1. **A new `[credential]` field, `store_only` (default `false`).** With it, the chain for
   any kind is the documented environment variable (if the route names one) and p1's own
   store, and NOTHING else. Without it, every source the kind had before is unchanged
   (the legacy chain), so every existing route file and test keeps its behaviour.
2. **`store_only` is a policy, not a kind.** For `api-key` it is exactly `borrow = []`.
   Setting it together with a non-empty `borrow` is a load error, because the two spell a
   contradiction. A route file still holds references only — never a secret.
3. **The policy is visible.** `p1 env show`, `p1 login --list` and `p1 models` print the
   same source line; a store-only route appends ` [p1 store only]`, and the CLI login is
   absent from the report's tried list and from the "what to do" guidance. An operator can
   see, without reading any credential, that no other tool's file is in play.
4. **Every shipped route is store-only:** `anthropic-subscription`,
   `openai-codex-subscription`, `glm-subscription`, `kimi-coding-subscription`,
   `opencode-go-subscription`, `opencode-go-2-subscription`. No shipped route reads Pi,
   OpenCode, Claude Code or Codex at runtime.
5. **`p1 login <route>` tells the truth for OAuth.** A store-only OAuth route is a usage
   error that says the credential is read from p1's own store, that `p1 login` cannot write
   an OAuth entry yet, that the CLI login is NOT read, and that p1 has no independent OAuth
   flow. It never claims a browser flow exists. A legacy OAuth route keeps its old "your
   login comes from the CLI" message.
6. **An independent OAuth grant is required for the Claude and Codex routes.** p1 must mint
   its own grant and store it in p1's 0600 store; importing a live refresh token from
   another CLI is explicitly rejected as the long-term mechanism. The Codex grant was
   transferred out of the retired Pi store by the XO and has passed a live p1 request; the
   Claude grant is still pending an owner login. No credential value is recorded here.

## Consequences

- No shipped route silently reads another tool's login file. A grep of `routes/*.toml` for
  `store_only = true` names the whole self-contained set.
- A self-contained route needs its entry in p1's store (or its documented environment
  variable). Until the OAuth acquisition step exists, the two OAuth routes need a grant
  placed in the store; `p1 login` cannot do that yet, and says so.
- Legacy routes are untouched: `store_only` absent means the exact chain, report and
  guidance as before, and that is regression-tested.
- The `store_only` policy travels with the route data into every consumer that builds a
  credential source (`p1-host`, `p1-usage`), because they all call `p1_auth::resolve` with
  the parsed `CredentialSpec`.
- A `store_only` route also stops the refresh write-back to another CLI: whatever it holds,
  it holds in p1's store.

## Alternatives considered

- **Rely on `borrow = []` alone.** Impossible for the OAuth kinds: they appended the CLI
  login unconditionally, independent of `borrow`.
- **A general `sources = ["env", "p1-store"]` list.** More interface than the one consumer
  needs, and it would let a route spell chains the chain does not implement. A boolean
  policy field is the smallest thing that closes the OAuth fallback.
- **Copy the active CLI refresh token into p1's store.** Rejected: rotation breaks the
  source CLI, and the order says p1 must own its grant.
- **A runtime flag that keeps borrowing available in the legacy chain.** Fails the order:
  the read would still be silent at runtime.
- **Ship a browser OAuth flow now.** Not done: the providers' authorization endpoints and
  redirect URIs are not available from any local primary source, and inventing them would
  be worse than saying the acquisition step is missing.

## Evidence

- `crates/p1-auth/src/spec.rs` (the `store_only` field and its validation),
  `crates/p1-auth/src/resolve.rs` (`CredentialPolicy`, the source list, the line marker).
- `crates/p1-auth/tests/store_only.rs`: a store-only chain never names or reads the CLI
  login, even when the CLI path is present (made unreadable so a read would be reported);
  an absent or rejected p1 entry is an error that names p1's store; the documented variable
  still wins; the legacy chain still reaches the CLI login.
- `crates/p1-host/tests/store_only.rs`: every shipped route is store-only (two tables);
  `p1 env show` and `p1 login --list` show `[p1 store only]`; the shipped Claude route reads
  p1's store and never falls back to a valid Claude Code login next to it; the shipped Codex
  route refreshes INTO p1's store and leaves the Codex file byte-identical; `p1 login` on a
  store-only OAuth route gives the new guidance while a legacy route keeps the old one.
- `crates/p1-host/tests/credentials_end_to_end.rs` loads the shipped route with the field
  removed, so the borrowed-source integration coverage survives under explicit legacy
  configuration.
- Live: the transferred Codex grant passed a p1 request (XO, 2026-09-24). Claude: pending.
