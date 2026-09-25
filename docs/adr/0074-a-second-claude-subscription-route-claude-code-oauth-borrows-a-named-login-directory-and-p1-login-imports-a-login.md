---
adr: 74
title: A second Claude subscription route: claude-code-oauth borrows a named login directory and p1 login imports a login
status: proposed
date: 2026-09-25
deciders: owner
supersedes: [61]
superseded_by: []
sources: [docs/design/credentials.md, docs/design/routes-and-profiles.md, crates/p1-auth/src/spec.rs, crates/p1-auth/src/locations.rs, crates/p1-auth/src/resolve.rs, crates/p1-auth/src/store.rs, crates/p1-host/src/login.rs, crates/p1-host/src/cli.rs, crates/p1-host/src/usage.rs, routes/anthropic-subscription-2.toml, environments/claude2/environment.toml]
---
# ADR-0074: A second Claude subscription route: claude-code-oauth borrows a named login directory and p1 login imports a login

## Context

Owner order, 2026-09-25 (issue #199): "implement a second claude code subscription
provider, so we can switch between them when our quota is reached so we don't screw up
during this sensitive migration."

A second Claude subscription is a second Claude Code login. Claude Code keeps a login in
its config directory (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) as `.credentials.json`; a
second account is logged in with `CLAUDE_CONFIG_DIR=~/.claude-2 claude`. Before this ADR a
`claude-code-oauth` route could only borrow the ONE default directory (ADR-0040's chain,
ADR-0061's login-file rule), and `p1 login` refused every OAuth route because p1 could not
write an OAuth entry into its store.

Two facts shape the decision:

1. ADR-0054 already counts an exhausted account (`UsageLimitExhausted`, a spent
   `RateLimited`, any provider failure) as a ROUTE failure that walks a workflow role's
   fallback chain. A second route is therefore all a `claude/… → claude2/…` failover needs;
   the engine does not change.
2. Some machines (an EC2 box) have no Claude Code at all; there the route can only read
   p1's own store, so a login has to be carried into it.

## Decision

1. **`login_dir` in `[credential]`.** A `claude-code-oauth` route may name the Claude Code
   config directory whose login it borrows: an absolute path, or a directory below the
   home (`~/<dir>`) that is expanded against the home directory when it is used. Absent
   keeps the default (`$CLAUDE_CONFIG_DIR`, else `~/.claude`). Any other kind with
   `login_dir`, a relative directory, or the home itself (`~`, `~/`) is a route-file load
   error. This amends ADR-0061's login-file rule only by
   naming the directory: the named login is read in place with the same lock, re-read and
   refresh write-back as the default one, and `store_only` still means it is never read.
2. **`p1 login <route> --from-claude-code [DIR]`.** For a `claude-code-oauth` route it
   copies the login in DIR (default: the route's `login_dir`, else the default directory)
   into p1's store under the route id as `{"type":"oauth","access","refresh","expires",
   "account_id"}` — `null` where the login records nothing; the account id is Claude Code's
   `oauthAccount.accountUuid` from `DIR/.claude.json` when that file has one. It goes
   through the store's own writer (0600 file, a wider store or directory refused, lock,
   atomic replace). A non-`claude-code-oauth` route, or a directory without a login, is a
   usage error naming the fix. The token is never an argument, and no output, log or error
   carries it. Importing again replaces the entry. Because p1's store precedes the borrowed
   login, an imported copy wins over the live login until `p1 logout <route>` removes it:
   `p1 logout` now removes a `claude-code-oauth` or `codex-oauth` route's store entry like an
   API key's (a missing entry on such a route is a usage error), and both the import and the
   plain-`p1 login` refusal say so.
3. **A second shipped route and environment.** `routes/anthropic-subscription-2.toml` is
   `anthropic-subscription` with its own id, its own origin
   (`anthropic-messages/claude-subscription-2`) and the credential
   `kind = "claude-code-oauth"`, `login_dir = "~/.claude-2"` — no `store_only`, so p1's
   store is tried first and the second account's Claude Code login after it.
   `environments/claude2` is `environments/claude` with `route = "anthropic-subscription-2"`,
   so `claude2/<profile>[:effort]` works wherever `claude/…` does. `p1 usage` labels the
   route `claude max 2` and probes it with its own credential.
4. **This supersedes ADR-0061, in three of its decisions** (`scripts/adr.py` records a
   reversal only as a whole-ADR supersession; the rest of ADR-0061 still stands):
   - ADR-0061 decision 4 ("every shipped route is store-only … no shipped route reads Pi,
     OpenCode, Claude Code or Codex at runtime") is replaced by: every shipped route is
     store-only EXCEPT `anthropic-subscription-2`, which reads the second account's Claude
     Code login in its `login_dir` at runtime.
   - ADR-0061 decision 5 ("`p1 login` cannot write an OAuth entry yet") is replaced by: a
     `claude-code-oauth` route's entry is written by `p1 login <route> --from-claude-code`,
     and `p1 logout <route>` removes an OAuth route's entry; `p1 login <route>` alone still
     reads no OAuth grant from stdin and still never pretends a browser flow exists.
   - ADR-0061 decision 6 ("importing a live refresh token from another CLI is explicitly
     rejected as the long-term mechanism") is replaced by: importing a Claude Code login is a
     supported, explicit operator action with the rotation risk stated (Consequences); minting
     an independent grant remains the open work of `docs/design/credentials.md` §8.4.
   - ADR-0061 decisions 1–3 STILL STAND unchanged: the `store_only` field and its meaning
     (env + p1's store only, the borrowed source never constructed), `store_only` as a policy
     that conflicts with a nonempty `borrow`, and the visible ` [p1 store only]` marker.

## Consequences

- A role `claude/…` with `fallback = ["claude2/…"]` moves a step to the second account
  when the first one's quota is exhausted, with no engine change.
- A bare profile name bound in both Claude environments (`claude-opus-5`) is ambiguous from
  any third environment and resolves to the current one from `claude` or `claude2`, exactly
  like the DeepSeek accounts; `environment/profile` is always unambiguous.
- An imported login is a COPY of a rotating refresh token (the risk ADR-0061 names): the
  first refresh on either side invalidates the other copy. The import is meant for a machine
  where only p1 uses that login (the EC2 case), or to be repeated after Claude Code rotated
  it. On the owner's own machine the second route needs no import: it borrows `~/.claude-2`
  in place.
- The second shipped route is the one shipped route that is not `store_only`, by owner
  order: its purpose is to read the second account's Claude Code login where it lives.
- `p1 login` on a `claude-code-oauth` route without the flag stays a usage error; it now
  names the import.

## Alternatives considered

- **Point `CLAUDE_CONFIG_DIR` at the second directory per run.** One process serves both
  routes in one workflow, so one environment variable cannot name two directories.
- **An engine-level "account switch".** Unneeded: ADR-0054's fallback chain already treats
  an exhausted account as a route failure.
- **Importing only (always `store_only`).** Rejected for the owner's machine: a copied
  refresh token and Claude Code's own copy would break each other at the first rotation.

## Evidence

- `crates/p1-auth/tests/login_dir.rs`: `login_dir` parsed, expanded and borrowed (absent →
  the default directory); a `login_dir` on any other kind is a load error; the import writes
  exactly the store's `oauth` shape, 0600 in a 0700 directory, refuses a wide directory, and
  no error carries a token.
- `crates/p1-host/tests/login.rs`: `--from-claude-code` through the host and the binary —
  the store shape, the default and explicit directory, the non-OAuth and missing-login usage
  errors, no token on stdout or stderr.
- `crates/p1-host/tests/workflow_claude_fallback.rs`: the shipped `claude` and `claude2`
  environments on one scripted transport; the first account answers `429 rate_limit_error`
  until its retries are spent, the step moves to `claude2/claude-sonnet-5` and runs with the
  second account's token.
- `crates/p1-host/src/usage.rs` test `usage_lists_both_claude_accounts`;
  `crates/p1-host/tests/anthropic_route.rs` `the_second_claude_route_is_the_first_on_its_own_account`.
