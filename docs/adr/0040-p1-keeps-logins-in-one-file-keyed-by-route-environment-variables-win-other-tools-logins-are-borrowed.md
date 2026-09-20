---
adr: 40
title: p1 keeps logins in one file keyed by route; environment variables win; other tools' logins are borrowed
status: accepted
date: 2026-09-20
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/design/notes/2026-09-20-credential-storage-research.md, crates/p1-provider-http/src/file_lock.rs, crates/p1-provider-anthropic/src/credentials.rs, crates/p1-provider-openai/src/credentials.rs]
---
# ADR-0040: p1 keeps logins in one file keyed by route; environment variables win; other tools' logins are borrowed

## Context

p1 reuses the Claude Code and Codex logins and is gaining routes (opencode-go, GLM, later
OpenRouter) whose keys live in other tools' files or in environment variables. Research into
how pi and opencode store credentials (note in sources; no credential value was read) found:
both use one 0600 JSON file keyed by provider id, neither uses the OS keyring; pi locks but
writes in place; opencode neither locks nor writes atomically. p1's adapters already do the
safer combination (non-blocking cross-process lock, re-read under the lock, atomic 0600 write).
Owner decisions, 2026-09-20: the proposed location and format are accepted ("We will have to
think about mac users … but that is for a later date"); precedence as recommended and as
opencode does it; "borrow what exists for now."

## Decision

- **Store:** `$XDG_CONFIG_HOME/p1/auth.json` (fallback `~/.config/p1/auth.json`), file
  0600, directory 0700 when p1 creates it; one JSON object keyed by ROUTE id; entries
  `api_key {key}` or `oauth {access, refresh, expires, account_id}` (pi's field names, so
  entries can be imported and exported). Reads and writes go through one small module using
  the existing `lock_exclusive` and atomic writer. Credentials belong to the route, never to
  a wire adapter or a model profile.
- **Precedence per route:** a documented environment variable, then p1's store, then the
  login of another tool the owner already uses (Claude Code, Codex, opencode, pi), then an
  error that names what to do. A refresh is written back to the source the credential came
  from, never to a different one.
- **Now:** borrow only. p1 gets no login command yet; its own store is specified but stays
  unused until a pasted-key login is built. Browser/OAuth logins stay borrowed from the
  official tools.
- **Later, not decided here:** macOS (paths, Keychain), an OS-keyring option behind a feature,
  `p1 login <route>` for pasted keys.

## Consequences

- Plain text on disk protected by file permissions — the same exposure the other tools already
  have on this machine; anything running as the user can read it.
- A forgotten variable in the shell profile silently overrides the file; the resolved
  environment shows WHICH source was used (never the value) so this is visible.
- Credential lookup leaves the adapter crates and lives in one place, which the provider split
  (ADR-0039) needs anyway.
- Credentials never reach logs, journals, `ResolvedEnvironment` or the shell tool's
  environment (unchanged rule).

## Alternatives considered

- OS keyring with file fallback: encrypts at rest, but needs a D-Bus stack and a running secret
  service, fails headless, and the plaintext fallback must exist anyway. Deferred, not rejected.
- `$XDG_DATA_HOME` (opencode's choice): no gain; credentials are not runtime data.
- The file beats the environment (pi): predictable, but a one-run override means editing the file.
- Own login command now: not needed while every route in use has a borrowed login.

## Evidence

`docs/design/notes/2026-09-20-credential-storage-research.md` (sources and observation
commands cited there).
