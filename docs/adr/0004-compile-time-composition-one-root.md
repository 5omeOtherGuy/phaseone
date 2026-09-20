---
adr: 4
title: Compile-time composition with one composition root
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/design-summary.md, docs/design/seams.md, AGENTS.md]
---
# ADR-0004: Compile-time composition with one composition root

## Context

`docs/design/design-summary.md` item 1 defines a module as "an independently selectable
implementation behind an explicit Rust interface, normally its own crate. Composed with
ordinary constructors at one composition root." `AGENTS.md` ("Architecture") keeps that rule
as a hard architecture rule.

## Decision

Modules are composed with ordinary constructors; `p1-host` is the single composition root
and the only crate that names concrete providers, tools, stores and providers. No plugin
loader, service locator, global registry or dependency-injection framework.

## Consequences

Composition is cheap and static; the compiler catches missing modules. Adding or removing
a module needs a rebuild. Tool input/output stays plain data, so an out-of-process or WASM
tool adapter remains possible later without changing the design.

## Alternatives considered

A runtime plugin system, or Iris issue #18's WASM/Extism loader. `docs/design/design-summary.md`
chooses "Compile-time modules over runtime plugins"; `seams.md` section 11 leaves the loader
behind.

## Evidence

`docs/SLICE-REPORT.md` acceptance 3: swapping or removing a tool/provider needs no edit to
the loop; `git log --stat -- crates/p1-core/src` shows the loop untouched by tool/provider
commits. Commit 2d24dae adds the host as the composition root.
