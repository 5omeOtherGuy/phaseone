---
adr: 129
title: Pluggable built-in agent tools and explicit reasoning off
status: accepted
date: 2026-10-08
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0129: Pluggable built-in agent tools and explicit reasoning off

## Context

The owner confirmed Search/Finder uses GPT-5.6 Terra at low effort, Librarian uses
GPT-5.6 Sol with reasoning off, and Task uses Opus 5.5 at medium effort. The owner
asked to reuse the ampi prompts and then clarified: "Make sure you build them as
wasm modules. Each one being a module you can plugin." Read Thread is parked.
Environment presets supply model and prompt configuration but are not tool modules.
An omitted reasoning effort leaves the server default; it does not mean reasoning off.

## Decision

1. Ship three separate tool-world components: `p1/finder`, `p1/librarian`, and
   `p1/task`, selected explicitly through `modules.lock` and an environment's tool
   entries. They are not auto-appended or native fallback implementations.
2. Each component uses the frozen `workers-start` and `workers-observe` capabilities
   to run its companion environment with a fixed grant and wait for its complete
   result. Inputs are `query` plus optional `context` for Finder/Librarian, and
   `prompt` plus `description` for Task. The components share guest execution logic,
   not runtime state. Standard worker scopes, journals and completion reports apply.
3. Companion environments pin the confirmed model/effort defaults. Their prompts
   adapt ampi's `src/extensions/ampi-workers/profiles/prompts.ts`: Finder's filename
   scanning uses Phaseone's grep; Task retains the donor worker role plus Phaseone
   tool/completion instructions. No Fable draft replaces the requested donor prompts.
4. Add the Responses-native Boolean option
   `openai-responses.reasoning_enabled`. Absent or true preserves existing behavior;
   false sends `reasoning: {"effort":"none"}` without requesting a reasoning summary.
   False with an explicit `reasoning_effort` is refused; non-Booleans are refused.
   The existing effort enum and frozen module ABI are unchanged.

## Consequences

- Plugins need their companion environment/prompt and the existing provider route.
  Loading a plugin alone cannot invent a model or credentials.
- Finder gets only read/grep plus finish. Task gets coding/verification tools plus
  finish. None can start nested workers; disabled worker capabilities remain disabled.
- GitHub-specific tools are not implemented in this slice. Librarian uses the
  existing shell with read-only `gh` instructions, plus read_output and finish.
  This restriction is prompt-level, not an enforced GitHub-only sandbox. Dedicated
  GET-only GitHub tools are separately owned work and replace this adaptation only
  when available and verified.
- Components return the started worker id plus its retained result/report. Resume
  reserves ids journalled under all three component identities.
- The later generic subagent configuration/options work keeps these owner-required
  separate plugins; it does not replace them with one generic tool.

## Alternatives considered

- Environment presets alone: insufficient for the owner's pluggable-module requirement.
- A new agent world or changed child-spec WIT: unnecessary; the existing worker
  capability already assembles the configured agent and owns its lifecycle.
- Treat omitted effort as off: incorrect because the server chooses its default.

## Evidence

Donor: [ampi prompt builders](https://github.com/5omeOtherGuy/ampi/blob/225c1c50a427f2a99552be4f5f6c63c16adc1e89/src/extensions/ampi-workers/profiles/prompts.ts).
Offline checks: `crates/p1-module-tests/tests/subagent_modules.rs` loads the built
components through the host, verifies fixed grants and returned answers, and refuses
nested linking; `crates/p1-assembly/tests/subagent_environments.rs` pins models,
reasoning policy and rendered roles. Responses request tests pin the explicit none
wire shape and conflicting/invalid option refusals. `lead_prompt_coherence.rs`
checks each shipped prompt against every tool subset.
