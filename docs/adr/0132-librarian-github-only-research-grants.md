---
adr: 132
title: Librarian GitHub-only research grants
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/adr/0129-pluggable-built-in-agent-tools-and-explicit-reasoning-off.md, docs/adr/0130-seven-github-research-components-over-a-host-held-get-capability.md, docs/adr/0131-configured-subagents-and-per-call-worker-options.md]
---
# ADR-0132: Librarian GitHub-only research grants

## Context

ADR-0129 temporarily granted Librarian shell/read_output because the GitHub
components were not available. ADR-0130 supplies seven independent research
components over a GET-only capability; ADR-0131 adds a configured subagent path
that must receive the same grants as the dedicated plugin. The owner requested
completion of the retained follow-ups: "Handle the follow-ups here please."

## Decision

Replace Librarian's shell/read_output grant with read_github,
list_directory_github, glob_github, search_github, commit_search, diff_github and
list_repositories in its Wasm plugin, companion environment and subagents.toml.
Restore the ampi prompt's GitHub-provider coverage, adapted to the shipped
schemas, pagination limits and finish contract; keep Sol/reasoning-off unchanged.

## Consequences

Librarian no longer has shell, general HTTP, local workspace or guest credential
access. GitHub reads use the host-held GET-only boundary rather than a prompt
restriction. Private repositories and code search require a suitable host-held
token; GitHub CLI login alone does not provide access.

A complete installed release containing all seven components is a prerequisite;
partial releases fail assembly. This change fulfills the temporary adaptation's
planned replacement without changing the worker WIT ABI or other subagents.
Reasoning-off wire/assembly coverage does not establish live server acceptance.

## Alternatives considered

Retaining shell access would preserve a broader capability despite the complete
GitHub release being available. Adding the GitHub components alongside shell
would not remove that capability. Neither meets the GitHub-only follow-up.

## Evidence

The published main-601cb97541b6 release was installed into an isolated prefix;
its binary reports that revision and `p1 modules verify` reports 37 verified
packages, including all seven GitHub components.

`crates/p1-assembly/tests/subagent_environments.rs` pins the exact companion
grants and rendered prompt. The shipped configuration test in
`crates/p1-host/src/catalog/subagents.rs` pins the configured grants;
`crates/p1-module-tests/tests/subagent_modules.rs` checks the separately built
component's actual child request, retained answer and cancellation.
