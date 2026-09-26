---
adr: 71
title: p1 migrates to WebAssembly modules: native core and host load tools, providers and policies by name
status: accepted
date: 2026-09-25
deciders: owner+lead
supersedes: [4]
superseded_by: []
sources: [issue #187, owner words 2026-09-25, docs/design/pillars.md, docs/design/design-summary.md, ADR-0004]
---
# ADR-0071: p1 migrates to WebAssembly modules: native core and host load tools, providers and policies by name

## Context

ADR-0004 chose compile-time composition: every module is a crate linked into the `p1`
binary and constructed at one root; a plugin loader was ruled out, and pillar 3 of
`docs/design/pillars.md` excluded "a runtime plugin system". ADR-0004 itself noted that a
WebAssembly or out-of-process tool adapter "remains possible later" because tool input and
output are plain data.

On 2026-09-25 the owner reviewed DeepSeek Harness (`dsh`, an everything-is-a-plugin
harness) against p1's compile-time model and decided the direction, in these words:

- "p1 is going to migrate completly to wasm."
- "The Core can be swappable, but that doesn't mean we want to be the ones swapping it.
  I am happy with our pi core." (the agent core stays native)
- "We do not need the browser GUI now, we can keep it in the terminal for now."
- "we will create an AWS ec2 machine to run the builds on. Just fot that project."

The lead's analysis before the decision (issue #187) found that p1's module cut already
matches dsh's seams one to one (provider, tool, persistence, subagent, compaction, sandbox,
approval, credentials); what differed was binding time only. The obstacles named were: the
contracts are async Rust traits with `Send` futures, cancellation tokens and a token stream;
WASI has no process spawn and no sockets; each module becomes its own build target and
release artifact; the runtime is a heavy dependency; a provider module receives credentials.

## Decision

p1 migrates to WebAssembly modules. The native binary keeps `p1-core` (loop and API),
`p1-contracts`, the host composition root, the journal, the worker service and the terminal
front ends. Every tool, provider, context policy and authorization policy becomes a
WebAssembly module built from its own crate, shipped in p1's release archive and loaded by
the host by name when an environment file assembles it.

The assembly rule stands unchanged: a module the environment does not name is never
instantiated and cannot dispatch. Capabilities a module cannot have under WASI (process
spawn for the shell tool, sockets for the WebSocket route, credential delivery, starting
workers) are host functions granted per module class by the host; the bubblewrap execution
boundary (ADR-0035) stays native. Composition stays explicit: the host names what it loads.
There is no marketplace, no auto-registration, no service locator and no global registry.

The runtime (wasmtime core modules, the component model or Extism), the interface form
(WIT or JSON strings), cancellation and streaming across the boundary, the host function
list, the build pipeline on the dedicated EC2 machine and the phase order are fixed by the
migration plan the owner commissioned from Astra (`gpt-6-astra`, xhigh, read-only; brief
and plan under `~/projects/phaseone-briefs/astra/wasm-migration/`) and by the ADRs that
plan produces. This ADR records the direction and supersedes ADR-0004's compile-time-only
rule; it does not fix those mechanisms.

## Consequences

- A tool, provider or policy can be added or removed without rebuilding the host; modules
  that need no escape run sandboxed by construction; modules may be written in other
  languages that target WebAssembly.
- A boundary layer beside `p1-contracts` and one host adapter per contract are new code;
  each module crate gains a `wasm32` build; the provider HTTP layer moves off `reqwest`
  onto what the boundary provides; the runtime adds binary size and build time. Builds run
  on the dedicated EC2 machine (owner); local cargo stays `check` only (ADR-0066).
- Until a later ADR says otherwise, the host loads only modules from p1's own release
  archive, verified by hash (ADR-0065 extended), so the credential rule holds for provider
  modules and `unsafe_code = "forbid"` stays; the plan must show the chosen runtime's API
  needs no `unsafe` on p1's side.
- ADRs the plan must revisit: 0015 (`Send` boxed futures at the boundary), 0017 (the
  conformance suite runs over the adapter), 0039 (the wire adapter becomes a module),
  0047 (WebSocket through a host function), 0065 and 0066 (release archive and CI gain
  module builds), 0068 (masking at the host side of the boundary). ADR-0002 stands: the
  core still depends only on contracts; the runtime lives in the host.
- `docs/design/pillars.md` pillar 3 boundary, `design-summary.md` item 1 and its
  "compile-time modules over runtime plugins" trade-off, `seams.md` sections 1 and 11,
  `assembly.md`, `AGENTS.md` Architecture, and the dated notes that restated the old rule
  (`iris-tui-reuse-plan.md`, `unified-dashboard.md`, `workflows.md`,
  `notes/2026-09-20-provider-split.md`) are amended in the same change.

## Alternatives considered

- Keep compile-time linking (ADR-0004): rejected by the owner.
- Dynamically loaded Rust libraries (`libloading`, `abi_stable`): rejected; Rust has no
  stable ABI, every call is `unsafe`, and unloading is unsound in practice.
- Out-of-process modules only (MCP-style servers over stdio): not the module system, but
  kept as a possible complement for tools where a process boundary is wanted.
- dsh parity including its browser shell and npm plugin ecosystem: out of scope; the
  owner keeps the terminal UI for now.

## Evidence

Owner words 2026-09-25 quoted above (DECISIONS.md D22). dsh survey of the same day: browser
client about 135k lines, host API about 25k, agent core about 15k, whole repository about
1M lines with tests (brain note 2026-09-25; two dsh docs copied beside the Astra brief). The
lead's partition and obstacle analysis: issue #187. The migration plan is pending from
Astra; its acceptance and the first proving module it names (the lead proposed the read
tool over the simplest interface the plan chooses, loaded by name) are the next evidence.

Acceptance (stream S1, issue #327): the read tool is the first real component and the
proving module named above. `p1/read` (`modules/p1-module-read`) is built by
`scripts/build-modules.sh --package p1-module-read`, loaded by name through the runtime
loader from the release manifest, and executed by the real host loader over the native
`workspace` and `snapshot` capabilities with output identical to the native tool's
(`crates/p1-module-tests/tests/read_module.rs`). It rests on S1.1 host-split (#229, merge
86fcf25), S1.2 workspace-read-exports (#233, cbfa1c4), S1.3 journal-version (#228,
6d6978f), S1.4 package-loader (#287, 74e47ee2, tagged `wasm-loader-v1`), S1.5
semantic-capabilities (#291, a25ddb0), S1.6 modules-cli (#286, bb8af75) and S1.8
read-component (the pull request closing #327). DoD commands: `scripts/build-modules.sh
--package p1-module-read`, `cargo test --locked -p p1-module-tests --test read_module`,
`cargo test --locked -p p1-tool-read`, `scripts/check-module-boundaries.sh`,
`cargo test --locked --workspace`, `scripts/gate.sh` and `python3 scripts/adr.py check`.
