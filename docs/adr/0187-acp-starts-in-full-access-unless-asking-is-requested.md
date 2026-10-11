---
adr: 187
title: ACP starts in full access unless asking is requested
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/adr/0038-full-access-is-the-default-asking-is-opt-in.md, crates/p1-host/src/frontend_port/mod.rs, crates/p1-host/tests/acp_fixtures.rs]
---
# ADR-0187: ACP starts in full access unless asking is requested

## Context

ADR-0038 makes full access the default, but the front-end port initialized every
ACP session in `ask`. Martty therefore requested approval for every tool call.
Owner, 2026-10-11: "Please implement dangerously-skip-permissions mode as default."
The existing ACP name is `full-access`; no new permission mode is needed.

## Decision

Initialize the front-end session mode from the run's flags before handing the
session to the client: `full-access` without `--ask`, `ask` with it. Both the ACP
legacy modes and the mode config option report that same starting value.

## Consequences

Default ACP sessions, including their workers, run tools without permission
requests. A client may still switch to `ask` or `read-only`; a run started with
`--ask` still cannot select full access. The sandbox remains fixed by `--sandbox`,
and workspace confinement and credential refusal remain unchanged. Full access
allows commands as the user; it is not a safe sandbox.

## Alternatives considered

Require Martty to select full access on every session: rejected because the host
must provide the requested default, consistently for every ACP client.

## Evidence

`cargo test -p p1-host --test acp_fixtures`: the default session advertises full
access through both ACP selectors and writes a file without a permission request.
Approval fixtures explicitly opt into `--ask`; the modes fixture retains checks
for read-only refusal and live switching back to ask.
