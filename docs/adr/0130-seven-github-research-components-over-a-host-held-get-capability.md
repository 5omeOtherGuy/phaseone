---
adr: 130
title: Seven GitHub research components over a host-held GET capability
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: [modules/wit/github-api.wit, modules/capabilities.toml, docs/design/github-tools.md]
---
# ADR-0130: Seven GitHub research components over a host-held GET capability

## Context

The owner requested ampi's seven GitHub-only research tools for p1, including fixes
for observed pagination and incomplete history-search behavior. After comparing
packaging choices, the owner chose: "Than build 1 wasm module per tool."
The existing tool world exports one declaration; tool modules have no HTTP service.
The Librarian agent needs remote research without an unrestricted shell grant.

## Decision

Ship seven independently grantable components sharing portable guest logic. Add
`github-api.get(path, raw-file)` to the tool capability allocation, with a fixed
GitHub origin, GET-only endpoint allowlist, host-held credentials, no redirects,
bounded responses and cancellation. Register the seven packages as release host
entries, selected explicitly by environments; do not add native tool fallbacks.
The provider transport world and its POST lowering contract remain unchanged.

## Consequences

Librarian can receive precise GitHub tool grants without shell access. Read-only
behavior is enforced by the host method/endpoint boundary, not a prompt convention.
The new import is additive: older components keep their existing imports, while
an older host refuses packages declaring the unknown capability. Components keep
distinct schemas and identities, at the cost of some binary duplication.
Credentials come from the host environment; code search/private access requires
an appropriate token. Explicit tool selection is the network opt-in.

## Alternatives considered

One dispatcher component: fits the singleton contract but loses independent tool
grants and precise schemas. One component exporting seven declarations: requires
a wider tool/runtime/catalog redesign. Shelling out to gh: would grant an unrelated
execution capability and make the research restriction prompt-only. General tool
HTTP: unnecessary for this slice; GitHub origin and endpoints stay fixed.

## Evidence

Donor: `5omeOtherGuy/ampi`, `src/extensions/ampi-github/`.
`crates/p1-github-guest/src/tests.rs` exercises offline pagination, range and query
boundaries; `crates/p1-module-tests/tests/github_tools.rs` loads and executes all
seven built components over scripted host responses. Native capability tests in
`crates/p1-module-runtime/src/github.rs` cover endpoint/origin refusal, response
bounds and cancellation. The contracts and remaining GitHub API limits are in
`docs/design/github-tools.md`.
