---
adr: 149
title: Pre-push tests shipped-data readers
status: accepted
date: 2026-10-09
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0149: Pre-push tests shipped-data readers

## Context

The pre-push selector mapped only changed package-owned files to tests. Shipped
route/account changes in #661 reached CI before `p1-usage`'s embedded-data test
ran. An environment addition in #663 likewise missed `p1-assembly` and host
inventories locally. Owner order on 2026-10-09: "please fix the pre-push script".
This extends the targeted local-check workflow of ADR-0128, not the CI gate.

## Decision

Also select workspace packages whose Rust `src/`, `tests/`, or `build.rs`
reference a changed top-level shipped-data directory. Derive readers at runtime
from Cargo metadata and path literals: relative embedding paths, manifest-root
paths, concatenated/formatted paths, and repository-root joins/helpers.

Keep deepest-manifest ownership for package file changes and the `modules/`
addition of `p1-module-tests`. Root-manifest-only changes still select no package.
Code/build/design directories (`crates`, `modules`, `scripts`, `docs`) and hidden
directories are not shipped-data directories. Other top-level directories need
no package inventory; deleted data files still select readers.

## Consequences

Workers see shipped-data inventory failures before pushing. Readers in inactive
code can conservatively select a package; ordinary directory labels and user
configuration paths do not. The scanner is lexical, not a Rust compiler or
dynamic path evaluator. It reads source only, never shipped credentials/prompts.
No workspace-wide pre-push expansion, dependency addition, or CI workflow change.
The existing gate's script-test step includes the new selector regression suite.

## Alternatives considered

A hand-kept directory-to-package table would drift when readers move or appear.
Whole-workspace testing would defeat targeted pre-push checks. A Rust syntax/data
flow analyzer is unnecessary for the current literal-based repository paths.

## Evidence

- `scripts/test_pre_push.py` checks fake changes to routes, accounts,
  environments and profiles against the real source tree, including host,
  usage and assembly consumers as applicable.
- Synthetic packages check embedding, build scripts, multiline joins, helpers,
  a previously unknown data directory, deletion, nested ownership, and exclusions.
- A stubbed full pre-push run asserts selected packages reach `cargo test -p`,
  without Rust compilation or whole-workspace testing.
- `scripts/pre_push_packages.py` is the shared selector used by pre-push and
  these command-level tests; there is no maintained production reader map.
