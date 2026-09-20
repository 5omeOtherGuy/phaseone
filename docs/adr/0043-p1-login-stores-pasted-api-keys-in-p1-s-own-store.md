---
adr: 43
title: p1 login stores pasted API keys in p1's own store
status: proposed
date: 2026-09-20
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/design/credentials.md, docs/adr/0040-p1-keeps-logins-in-one-file-keyed-by-route-environment-variables-win-other-tools-logins-are-borrowed.md]
---
# ADR-0043: p1 login stores pasted API keys in p1's own store

## Context

ADR-0040 specified p1's own credential store but left it unused: "p1 gets no login command yet".
The same day the owner's primary OpenCode Go account ran out of credit and a second account had
to be wired in; its key lives in a plain file that no borrowed login store knows, so p1 could
only reach it through an environment variable set by hand at every dispatch. Asked whether the
store should stay in the credential chain, the owner answered (2026-09-20): "Keep the store. You
can dispatch a worker to work on the login implementation."

## Decision

p1 gets `p1 login <route>`, `p1 login --list` and `p1 logout <route>` for routes whose credential
kind is `api-key`. The key is read from stdin (hidden on a TTY), never from an argument, and is
written to `~/.config/p1/auth.json` under the lock, atomically, 0600 in a 0700 directory; wider
permissions are refused, not repaired. Nothing is verified against the network. OAuth logins
stay borrowed from the official tools (`docs/design/credentials.md` §6).

## Consequences

- A new account is `routes/<id>.toml` plus `p1 login <id>`: no variable to remember, no other
  tool's store to edit.
- p1 now WRITES a secret to disk — plain text under file permissions, the exposure ADR-0040
  already accepted for reading. Everything running as the user can read it.
- The precedence of ADR-0040 is unchanged: a forgotten environment variable still overrides the
  store; `login` and `--list` say so when it happens.

## Alternatives considered

- Keep using environment variables: every dispatch has to carry the key; easy to get wrong.
- A key-file reference in the route file (`file = "~/.config/keys/x.key"`): a third place where
  secrets may live, and route files are committed — a path there invites committing the secret's
  location conventions of one machine.
- An OS keyring: later, behind a feature (ADR-0040), not needed to solve today's problem.

## Evidence

`docs/dogfood/runs.jsonl` (`split4b-deepseek2`): the second account worked only through
`OPENCODE_GO_2_API_KEY` supplied at dispatch. Owner messages of 2026-09-20.
