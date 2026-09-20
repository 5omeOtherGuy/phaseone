---
adr: 16
title: Flat ordered history and raw tool input validated at the tool
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [D17, docs/design/routes.md, docs/design/seams.md]
---
# ADR-0016: Flat ordered history and raw tool input validated at the tool

## Context

D17 (lead). `docs/design/routes.md` section C records what the two real routes force: the
Claude route alternates user/assistant messages with coalesced blocks, the Codex route is a
flat item list; a call's arguments are a JSON object on Claude but a JSON string or raw
freeform text on Codex.

## Decision

Core history is a flat ordered item list with ordered assistant blocks. Tool input is
`ToolInput::Json(raw)` or `ToolInput::Text(raw)`, preserved raw and validated only at the
tool's own boundary. Declarations are `ToolDeclaration::Function { schema }` or
`Freeform { grammar }`; providers translate the shapes they support.

## Consequences

The core never parses tool input, and invalid input becomes an explicit tool error, not
an application crash or a guessed repair. A provider that needs coalesced messages does that
translation itself (route A), keeping the core route-neutral.

## Alternatives considered

Normalising every route onto one message shape, or validating JSON in the core. The
routes table (`routes.md` section C) shows both would lose native behaviour; `seams.md`
section 3 requires raw arguments to be preserved until a complete call exists.

## Evidence

`docs/design/routes.md` section C table. The conformance check
`invalid_tool_json_is_preserved_raw` (check 5) proves the raw invalid text is surfaced
unchanged; see `docs/design/providers.md`.
