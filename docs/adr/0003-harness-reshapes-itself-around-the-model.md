---
adr: 3
title: The harness reshapes itself around the model
status: accepted
date: 2026-09-19
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [D1, docs/design/pillars.md, docs/design/assembly.md, docs/design/seams.md]
---
# ADR-0003: The harness reshapes itself around the model

## Context

Owner direction (D1): the "harness reshapes around the model (Claude and GPT first)".
Pillar 2 states the result: an agent "gets the prompt, tools, tool descriptions and provider
behaviour suited to its model — and sees only those." The lead specified the mechanism in
`assembly.md` and `seams.md` section 5.

## Decision

One resolved selection (route, model, task/config) drives BOTH the prompt and the exact
tool set. Environments are files (`environments/<name>/environment.toml` + a whole
`prompt.md`); tools OFFER implementations, configuration CHOOSES, providers VALIDATE. An
agent owns only its assembled tools and cannot dispatch anything else.

## Consequences

An agent sees only its own prompt and tools; a new tool must be named in configuration
before a model can use it; incompatible combinations fail before the run starts. There is no
general prompt-fragment engine.

## Alternatives considered

A generic toolbox with names hidden from the model; tools auto-registering themselves
(rejected in `design-summary.md`: "Configuration chooses tools per model ... over tools
auto-registering themselves"); Iris's fragment frontmatter/slot machinery (left behind in
`seams.md` section 11).

## Evidence

`docs/SLICE-REPORT.md` acceptance 2: `p1-host env show claude` lists read/edit/write/grep/shell,
`env show gpt` lists shell/apply_patch (freeform). The same report shows the Claude route used
read/edit/write/shell live while the GPT route used shell/apply_patch only. Commits 4293a9a
(assembly) and 7c4203c (environment files).
