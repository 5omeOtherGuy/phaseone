---
adr: 2
title: One small core with tools and providers as modules
status: accepted
date: 2026-09-19
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [D1, D4, docs/design/seams.md, scripts/check-core-isolation.sh]
---
# ADR-0002: One small core with tools and providers as modules

## Context

Owner direction (D1): "One small core + modules; tools are their own modules; providers
translate only." The lead fixed the enforcement (D4): core isolation must be "a runnable
command, not a convention" (`seams.md` section 10).

## Decision

`p1-core` runs one agent loop and depends only on `p1-contracts`. Every tool is its own
crate; providers translate wire behaviour only and contain no tools. `scripts/check-core-isolation.sh`
asserts on the resolved `cargo tree` that the core has no concrete module, HTTP/TLS, terminal
or storage dependency, and runs inside the gate.

## Consequences

The core stays small and provider-, tool- and format-neutral. Adding a module means a
rebuild, not a runtime registration. The isolation check is part of green, so a leaking
dependency fails the gate (D4).

## Alternatives considered

A monolithic crate, or a convention that the core "should not" name modules. D4 chose the
script precisely because the first acceptance item had to be runnable.

## Evidence

`docs/SLICE-REPORT.md` acceptance 1: `scripts/check-core-isolation.sh` prints
`core isolation: ok` (p1-core graph: p1-contracts + tokio/futures/thiserror, 29 packages),
and `cargo test -p p1-core` runs 112 tests against scripted fakes only. See
`../../scripts/check-core-isolation.sh`.
