---
adr: 20
title: Subscription routes reuse the existing CLI logins
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/routes.md, docs/design/providers.md, docs/SLICE-REPORT.md]
---
# ADR-0020: Subscription routes reuse the existing CLI logins

## Context

Both first-slice routes are subscription (OAuth) routes, not public API-key routes. The
donor kept its own credential store and login flow; `routes.md` records the p1 change and
`providers.md` specifies the file handling.

## Decision

p1 builds no login flow in this slice. Both adapters read the owner's existing CLI login
fresh on each access: Claude Code's `$CLAUDE_CONFIG_DIR/.credentials.json` else
`~/.claude/.credentials.json`, Codex's `$CODEX_HOME/auth.json` else `~/.codex/auth.json`.
Refresh happens only when expired or rejected; because refresh tokens rotate, the new token
set is written back to the same file under an advisory sibling `.lock`, re-reading first,
atomically (temp + rename, mode 0600), preserving unknown fields. A 401/403 forces one
refresh, once.

## Consequences

No credential is ever copied, dumped or journalled, and other tools (Claude Code, the
Codex CLI) keep working because the same file is updated in place. The lock and atomic write
handle the race. A known weakness: refresh takes a blocking file lock on the runtime thread,
and refresh was not exercised live in the slice (tokens were valid).

## Alternatives considered

The donor's own credential store plus a login flow. `routes.md` marks this as
"donor→p1 change": the donor kept `~/.iris/auth.json` and its login flow, which p1 dropped.

## Evidence

`docs/design/routes.md` auth sections for both routes; `docs/design/providers.md`
("Credentials (file-based sources live in the adapter crates)"). `docs/SLICE-REPORT.md`
("What is weak") records the blocking-lock and unexercised-refresh caveats. Commits 65492cc
(Claude adapter) and c744f3f (Codex adapter).
