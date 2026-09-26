---
adr: 83
title: Privileged process and completion capabilities
status: proposed
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [epic #206, issue #224, issue #254, PR #232, DECISIONS.md D22, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0081-native-foundation-and-runtime-components.md, docs/adr/0082-component-abi-and-execution-ownership.md, docs/adr/0035-the-shell-tool-can-run-inside-a-bubblewrap-execution-boundary.md, docs/adr/0037-unattended-runs-end-by-an-observable-finish-call-with-bounded-continuation.md, docs/adr/0038-full-access-is-the-default-asking-is-opt-in.md, docs/adr/0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md, docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, docs/adr/0055-a-successful-command-that-changes-the-workspace-counts-as-progress-for-the-stall-guard.md, docs/adr/0068-tool-output-is-masked-for-credential-shapes-before-history-journal-and-summaries.md, docs/adr/0077-builds-on-the-stream-boxes.md, docs/adr/0078-connection-resources-and-component-replacement.md, docs/design/completion.md, docs/design/modules/README.md, docs/design/modules/wit.md, docs/design/modules/package.md, docs/design/modules/capabilities.md, docs/design/modules/adapters.md, docs/design/modules/cancellation.md, modules/wit/process.wit, modules/wit/session.wit, modules/wit/worlds.wit, modules/wit/runtime.wit, crates/p1-module-runtime/src/capabilities.rs, crates/p1-module-runtime/src/tool.rs, crates/p1-tool-shell/src/process/mod.rs, crates/p1-host/src/activity.rs, crates/p1-host/src/catalog/tools.rs]
---
# ADR-0083: Privileged process and completion capabilities

## Context

ADR-0071 moves every tool into a WebAssembly component the host loads by name, and
[ADR-0081](0081-native-foundation-and-runtime-components.md) already decides that enforcement
and OS services stay native: the bubblewrap execution boundary and the process service
extracted from `p1-tool-shell` are in its table of native components. Two tools hold more
authority than the rest, and their authority was placed inside the tool crate:

- `shell` runs arbitrary commands. ADR-0035 put the bubblewrap execution boundary in
  `p1-tool-shell`: the mount plan and its order, the credential masks, the PID namespace, the
  private `/tmp`, the probe that fails assembly; the environment allow-list (issue #4) and the
  process-group kill with escalation live there too. ADR-0038 names the allow-list and the
  sandbox as the boundaries that remain under the full-access default.
- `finish` decides that an unattended run is done. ADR-0037 has the tool check `done` against
  the session's recorded shell runs and the last file change, and store the accepted outcome in
  a cell the host reads; ADR-0051 lets the host choose a second completion policy for an agent
  with no tool that records command runs, from the assembled tools' identities, and makes the
  evidence host-owned.

A component is untrusted code in this design: the host loads only p1's own release packages,
verified by digest ([ADR-0082](0082-component-abi-and-execution-ownership.md)), but a
component's behaviour is not what the host relies on for a boundary. If the execution boundary
or the accepted completion state stayed inside the component, a component could run a command
outside the sandbox or declare a run done that no command verified.

The frozen boundary (`wasm-boundary-v1`; [`wit.md`](../design/modules/wit.md), freeze items 1,
3, 4, 10 and 13) gives these two tools capabilities instead: `process` (`spawn` a `bash -lc`
command with a time limit; the `running` streaming resource), owned by the process service
extracted from `p1-tool-shell`, and `completion` (the session record, the policy, the output
contract and `accept`), owned by the host completion hub. `process` is in the `tool` row of the
allocation only. `completion` is in the `tool` and `context-policy` rows
(`modules/capabilities.toml`; [`wit.md`](../design/modules/wit.md)): a context policy may read the
record, while the commit rules below restrict committing to the finish component's `execute` call.
This ADR records where each piece of authority now lives and what the host
verifies, so that no component can move it. It describes the frozen boundary and changes none
of it.

ADR-0068 masks credential shapes in tool output before history, the journal and summaries. A
component also produces text that did not exist natively as a separate channel: its
declaration and descriptions, and the diagnostics a failed or trapped call carries across the
boundary.

## Decision

### 1. The process capability (ADR-0035 placement)

The execution boundary is the native process service in `p1-tool-shell` (`ProcessService` in
`crates/p1-tool-shell/src/process/`, S3.1, PR #232), not the shell tool. The `process` import is
linked to the runtime's `ProcessService` and `RunningProcess` traits
(`crates/p1-module-runtime/src/capabilities.rs`), which the host implements over that native
service; the runtime only adapts it. The service is assembled natively, per assembly, from the
workspace root, the environment snapshot, the `--env-pass` names and, when the host selects it,
the bubblewrap `Sandbox`, whose assembly fails when the sandbox is unusable. A request carries
only the command text and its time limit (`process.command { script, timeout-ms }`); nothing in
it chooses the program, the environment, the working directory or the sandbox. Native,
unchanged:

- the mount plan and its order, the credential-directory refusal for `--sandbox-read`, the
  cargo token masks after every writable bind, the private `/tmp`, `--unshare-pid` and
  `--die-with-parent`;
- the environment policy: cleared, then rebuilt from `ENV_ALLOW`, `ENV_ALLOW_PREFIXES` and the
  `--env-pass` names, so a command never inherits p1's environment;
- process lifetime: `bash -lc` in the workspace root, stdin closed, no terminal, its own process
  group;
- output capture: the bounded head/tail capture of standard output and standard error, merged in
  arrival order, applied natively before the bytes reach `running.next`;
- termination: on timeout and on cancellation, SIGTERM to the group, the grace period, SIGKILL
  escalation and reaping, completed before the call returns (`exited(timed-out)`,
  `exited(cancelled)`); dropping the `running` resource, the end of the export call that created
  it and an abandoned call end the group too, because the runtime drops the handle and a
  `RunningProcess` must kill the group on drop. A trap never undoes what the command did
  ([`cancellation.md`](../design/modules/cancellation.md), ADR-0082).

Guest behaviour in the shell component: input validation, the declaration and description,
`effect` and `describe` (destructiveness classification, on the restricted path with no
capability linked), the output filters and result formatting, `describe-result`. Its manifest
grants `process` and `clock` (for `monotonic-now`, the elapsed time it reports) and nothing
else; a grant is per interface, so `clock.now` comes with it. It has no `control`: a
cancellation reaches it through `running.next`, which returns what remains and then
`exited(cancelled)`.

The sandbox paragraph the model reads (ADR-0035: "the description the model sees says what the
boundary is") and the `+sandbox` identity variant describe the boundary, so they come from the
side that owns it. The `tool` world has no `configure`, and "an environment's tool face is
applied by the host" ([`wit.md`](../design/modules/wit.md), additions to the worlds); the tag's
loader builds the identity from the manifest `name` and `variant` only and specifies no other
mechanism ([`package.md`](../design/modules/package.md)). Therefore the host's shell entry
(`crates/p1-host/src/catalog/tools.rs`, S3's, slice S3.8) applies the paragraph and the variant
when it assembles the component over a sandboxed process service, before the environment's
face as today: the description gains a newline and the paragraph, the variant gains `+sandbox`,
both byte-identical to today's text.

### 2. The completion capability (ADR-0037 and ADR-0051 placement)

The accepted completion state is the host's. The host completion hub (`CompletionHub`,
`crates/p1-host/src/activity.rs`) owns, per agent, the activity record (fed by the host from the
event stream, the workspace fingerprint of ADR-0055 and the journal on resume; a component never
feeds it), the completion policy, the output contract and the accepted cell, and backs the
`completion` capability. The `finish` component reads the record through `completion`
(`last-file-change`, `shell-runs`, `policy`, `output-contract`), checks a call and formats the
texts of that call as today, and submits a candidate with `accept`. A submission is a candidate,
not a commit: the hub re-verifies every candidate against its own record before it commits it,
by these rules:

1. **`done` with `commands-passed(list)`** commits only if the list is non-empty and every
   command, normalised as [`completion.md`](../design/completion.md) §2 says (trim, collapse
   whitespace, one leading `cd <path> &&` dropped), equals the command of a run in the hub's
   record (rule 2) whose LAST run succeeded (exit code 0) — a failing re-run invalidates an
   earlier success — and that is neither piped nor exit-masked and is newer than the last file
   change, where a successful command that changed the workspace fingerprint is a file change
   (ADR-0055). A command no qualifying run matches, a fake command included, a command whose last
   run failed and a command older than the last file change are refused. The committed evidence is
   the hub's own list.
2. **Only a tool whose loader-built identity carries the `records-command-evidence` capability
   produces evidence** (the shell component). `shell-runs` reports every finished `executes`
   call as the frozen WIT says, but the hub counts as evidence only the runs of such a tool; a
   component cannot append a run, and a call of any other tool, whatever its name, its face or
   the effect it classified, never counts. `records-command-evidence` is a host fact about a
   loader-built identity: it is not a `p1:module` interface of the frozen allocation
   (`modules/capabilities.toml`), so no manifest can grant it, and the identity is built by the
   loader from the manifest, so no component can claim it (ADR-0082).
3. **`done` with `not-run(reason)`** commits only under `report-to-parent`, or under
   `recorded-commands` when the record holds no file change. The committed reason is the
   hub's (`no command tool granted` or `no file changed`), never the component's text.
4. **`blocked`** commits only with non-empty `needs`.
5. **A structured result** is checked by the hub against the output contract it holds: it
   computes the `schema-check` itself and commits its own verdict, not the component's claim. A
   `done` without a value when a contract is set is refused; a value that fails the check is
   still an accepted `done` with the errors (ADR-0053 item 5).
6. **Freshness.** A candidate commits only within an `execute` call of the finish component
   the current assembly granted `completion`. A candidate from before a re-grant (an instance of
   an earlier assembly, or a call that started before it) cannot commit; neither can a call on
   the restricted path, where no capability is linked. The last committed candidate of a turn
   wins, and the committed state is cleared when a turn starts, as today.
7. **The policy is the host's.** The hub selects it at every assembly boundary, including
   every worker re-grant, from the assembled tools' loader-built identities: `recorded-commands`
   when some assembled tool carries `records-command-evidence`, `report-to-parent` otherwise;
   main agents always get `recorded-commands`. `completion.policy` reports the hub's choice; a
   component cannot choose or change it.

The frozen `accept` returns nothing (`modules/wit/session.wit`, interface `completion`), so the
component never learns the hub's decision. The host adapter of a component granted `completion`
observes the hub's decision after `execute` returns and, when the hub refused the call's
candidate, replaces the call's outcome by an ordinary tool error naming the rule, so the model
reads it in the same turn exactly as it reads the tool's own rejections today. This is generic
for any component granted `completion` and names no tool. Tool adapters are S0's (`WasmTool`,
freeze item 12; streams implement components, not adapters, ADR-0082), so if the adapter
cannot do that without a WIT change, S3.7 raises a BLOCKERS entry to S1.

The finish declaration depends on the policy and the output contract (the report-to-parent
description; the contract's paragraph and `result` parameter). The frozen adapter reads a
component's declaration once, at construction, on the restricted path, where `completion` is not
linked ([`adapters.md`](../design/modules/adapters.md)); so, as for the sandbox paragraph, the
host's finish entry applies the policy's and the contract's declaration when it assembles the
component, byte-identical to today's text, and S3.7 raises a BLOCKERS entry to S1 if that cannot
be done inside the frozen boundary.

The host policy of ADR-0037 is unchanged: it judges each completed turn from the hub's committed
state only (done → exit 0; blocked → exit 3; waiting; at most three continuations, then exit 4),
and the worker report takes the evidence from the committed state (ADR-0051 item 3).

### 3. Defaults (ADR-0038 preserved)

Full access stays the default and asking stays opt-in (`--ask`). The sandbox stays off by
default and on with `--sandbox workspace`; `--sandbox-read`, `--sandbox-write` and `--env-pass`
keep their meaning. The environment allow-list stays always on. These are chosen by the host at
assembly; no component, manifest or module setting can change them: none can turn the sandbox
off, widen the allow-list or change the working directory.

### 4. Masking at the boundary (ADR-0068 preserved and extended)

`RedactingTool` wraps every assembled tool adapter; `wasm_tool` returns a component's `WasmTool`
already wrapped, and an unwrapped one is never handed out (ADR-0082,
[`adapters.md`](../design/modules/adapters.md)). Masking stays before anything durable is built,
as ADR-0068 says. At the tag it masks a tool outcome's text; at the component boundary it is
extended (S3.6) to:

- module descriptions: the declaration's description and the call and result descriptions a
  component returns (they reach the model, the journal and the TUI);
- the safe diagnostics that cross the boundary: a module failure's message, a trap's text, a
  process spawn error, an assembly error naming a module, and the reason of a refused
  completion candidate;
- summary inputs: the transcript a context policy sends through `summary` is rendered from
  history that is already masked, and the host masks the summary text before the module sees it,
  as the frozen `summary` interface states.

Opaque replay payloads are left unmasked: they are version-tagged provider data that must
round-trip byte for byte, they are not shown to the model as text, and masking them would break
replay (PLAN.md §9); `p1-redact` already never passes a provider's thinking signature to the
matcher. A component that replaces another during a worker re-grant is assembled through the
same constructor and wrapped at the same assembly step, installed only at an assembly boundary
between complete turns (ADR-0078 §4), so no re-granted tool dispatches unwrapped.

## Consequences

- The shell component can run commands only through the host's boundary: a component cannot
  leave the sandbox the host assembled, cannot see p1's environment and cannot outlive its call.
- A `finish` component cannot fabricate completion: fake commands, commands older than the last
  file change, runs of a tool without `records-command-evidence` and candidates from before a
  re-grant are refused by the hub, and headless exit codes depend only on the hub's committed
  state.
- The shell tool's description and identity depend on the process grant, so an environment
  that grants a sandboxed `process` gets the sandbox paragraph without the component knowing.
- The model-visible text of a refused candidate comes from the host, not the component; the
  refusal path depends on the tool adapter, which is S0's, and may need S1.
- More native code stays in `p1-tool-shell` and `p1-host` than the other tools keep; that is
  the price of keeping the two privileged boundaries native.
- The real-bubblewrap cases run only where the bwrap probe succeeds: on the stream boxes;
  GitHub-hosted runners skip them with a printed SKIP (ADR-0077), so the box run is the
  evidence for them.

## Alternatives considered

- Keep the boundary inside the shell component, with bubblewrap arguments passed through an
  import: a component could pass a weaker mount plan or none; rejected (ADR-0081).
- Give the component WASI process or environment access: the guest target is
  `wasm32-unknown-unknown` with no `wasi:` import, and every `wasi:` import is refused (D-XO-4,
  ADR-0081); WASI would also move the environment policy into the guest; rejected.
- Let `finish` write the accepted state itself (the component's `accept` is final): a component
  could declare done without evidence; rejected.
- Move command verification into the host entirely and drop the finish component: loses the
  guest's ownership of the model-visible texts of a call and of the output-contract formatting
  of ADR-0053 item 5; the hub re-verifies instead of re-implementing the tool.
- Mask opaque replay payloads too: breaks byte-exact replay; rejected (PLAN.md §9).

## Evidence

What exists now: S3.1, the native process-service extraction — PR #232, merge commit
`ef6aa31`, main's `gate` run 36178525646; the frozen boundary this ADR describes is the tag
`wasm-boundary-v1`. The rest is proved by the S3 slices' checks, recorded with each slice's PR,
merge commit and evidence bundle: `cargo test --locked -p p1-tool-shell -p p1-tool-finish`;
`P1_REQUIRE_BWRAP=1 cargo test --locked -p p1-module-tests --test shell_boundary` on box
wasm-s3 with no skipped case; `cargo test --locked -p p1-module-tests --test effect_settlement`,
`--test redaction` and `--test finish_boundary` (fake command, old command and re-grant cases);
`scripts/gate.sh` on the box. This section is completed when the ADR is accepted, in the PR that
lands the phase's last definition-of-done row (ADR-0078).
