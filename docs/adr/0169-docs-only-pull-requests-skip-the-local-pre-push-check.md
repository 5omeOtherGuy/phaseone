---
adr: 169
title: Docs-only pull requests skip the local pre-push check
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0169: Docs-only pull requests skip the local pre-push check

## Context

`scripts/pre-push.sh` selects the tests of packages whose sources read a changed shipped-data
directory, so a Markdown file added under `environments/` (#769) started a cold workspace build
before a one-file README could be pushed. The owner said: "Just push the readme no need for a
check. It's jsut a text file", then: "You can make this a rule for all documentation."

## Decision

A pull request whose every changed file is Markdown documentation is pushed without running
`scripts/pre-push.sh`; the gate's docs-only fast path (ADR-0128) is its only check. A
`prompt.md` under `environments/` is assembly input, not documentation: a change to one still
runs `cargo test -p p1-assembly` before the push.

## Consequences

- Documentation lands in minutes with no local build.
- A broken relative link or wrong path in a document is caught by no local check; the CI light
  gate still runs the ADR check and the script tests.
- Prompt edits keep their coherence test locally, because the CI fast path classifies them as
  Markdown and skips it.

## Alternatives considered

Teaching `pre-push.sh` to skip Markdown-only diffs: a script change for what is a working rule;
not requested.

## Evidence

Pull request #770 (README only) merged through the docs-only fast path with no local run.
