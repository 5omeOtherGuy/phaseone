---
adr: 84
title: Runtime delegation and workflow modules
status: proposed
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [epic #206, DECISIONS.md D22, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0050-every-main-agent-can-start-workers-a-worker-gets-exactly-the-tools-its-parent-grants.md, docs/adr/0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md, docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, docs/adr/0054-workflow-roles-have-a-fallback-chain-for-route-failures-deepseek-is-the-shipped-worker.md, docs/adr/0081-native-foundation-and-runtime-components.md, docs/adr/0082-component-abi-and-execution-ownership.md, docs/design/modules/wit.md, modules/wit/delegation.wit, modules/wit/worlds.wit, docs/design/delegation.md, docs/design/workflows.md]
---
# ADR-0084: Runtime delegation and workflow modules

## Context

ADR-0071 (DECISIONS.md D22) moves every tool, provider and policy into WebAssembly modules
that the host loads by name. The agent core, the host, the journal and the worker service stay
native. Delegation and workflows are not a single tool:

- ADR-0050 puts the four worker tools on every main agent. It also decides which tools a child
  gets, what `worker_result` reports, and how a re-grant reassembles a child between turns.
- ADR-0053 makes workflows a separate family. It has four tools, a sandboxed Rhai engine, roles
  and caps, and a run journal with replay.
- ADR-0051 and ADR-0054 fix how a worker's completion is labelled and when a workflow step moves
  down its role's fallback chain.

All four assume compile-time composition. The `delegation` and `workflows` cargo features were
the only way to remove the tools, and the host appended all four tools of a family at once.
Moving these families behind the boundary raises three questions:

1. What a tool component may do to other agents. A component must not reach an agent it did not
   start, whatever ids it supplies.
2. Where Rhai stops and a component starts. The interpreter re-enters the host on every
   `agent()`, `parallel()` and `pipeline()` callback. A component invocation that stayed
   suspended across such a callback would re-enter its own Store.
3. How a user removes the families. A cargo feature cannot be the answer once p1 ships one
   binary with modules chosen at run time.

## Decision

1. **Native stays native.** The agent lifecycle, the worker service (`p1-workers`), the Rhai
   interpreter and its thread pool, workflow run state, attempt counters, cap enforcement,
   journal ordering and every handle table stay in the native host. None of them moves into a
   component.
2. **One package per member (D045).** The eight tools are eight module packages:
   `modules/p1-module-worker-start/`, `-worker-result/`, `-worker-continue/` and
   `-worker-cancel/`, and `modules/p1-module-workflow-start/`, `-workflow-status/`,
   `-workflow-result/` and `-workflow-cancel/`. A family is the host's list, not a package:
   `WORKER_MODULES` and `WORKFLOW_MODULES` name the member module ids, not native
   implementations. The host assembles each member by id. A member that is not assembled is not
   instantiated and cannot dispatch, and selecting `worker_result` neither assembles
   `worker_start` nor grants its capability.
3. **Member-scoped capabilities.** A member receives only its own worker or workflow operations
   (for example, `worker_result` gets status, wait and describe, and never start). Following
   S0-R1.3, the worker operations are three interfaces: `workers-start`, `workers-observe` and
   `workers-control`, with the types in `worker-types`. A member's grant is therefore a static
   fact of its component's imports, and `check-module-boundaries.sh` checks it. `workflows`
   stays one interface, so the host's link for a workflow member refuses the operations that
   member does not own (`workflow-error::preflight("not granted: <operation>")`).
4. **Scoped ids.** Every worker or run id a component sees is valid only within the calling
   instance's assembly generation, operation and parent agent. Any other id, a dropped one or a
   stale one is `unknown-child` / `unknown-run`. A component cannot select another agent by
   supplying its id. Following S0-R1.2, worker ids stay host-scoped strings and no WIT
   resource represents a child; run ids are likewise strings of the `workflows` interface, and
   this ADR gives them the same scope.
5. **Workflow decisions are short component calls over a native substrate.** The decision logic
   moves behind two synchronous calls: `plan-step(snapshot, request)` and
   `accept-step(snapshot, outcome)`. That logic covers role and fallback selection, building the
   step envelope, repair decisions and replay matching. Each call returns a transition, and the
   native run service validates it before applying it. The decision component imports only
   `control` and `clock`. No component invocation stays live while a Rhai callback runs, so no
   Store is re-entered. Following S0-R1.1, this is the `workflow-decision` world. The native
   substrate keeps all state and passes the snapshot in, and `workflow-implementation` stays
   available for workflows written as modules. Until the loaded decision component lands
   (package `modules/p1-module-workflow-decision/`, kind `workflow-decision`), the native
   `Decisions` implementation answers the same two calls.
6. **Runtime disablement is the normal way to remove the families.** It replaces compile-feature
   absence as the user mechanism. Disabling the workers or the workflow capability removes the
   family's tools and their prompt sections from the next assembly (ADR-0050 item 4's
   conditional sections). Children and workflow runs already running keep the generation they
   were started with. The disabled path produces an explicit disabled-feature or assembly error,
   never an implicit fallback.
7. **Preserved decisions.**
   - ADR-0051 stands: the completion policy and the evidence label are chosen by the native host
     from the assembled tools' identities. A component never produces or relabels evidence.
   - ADR-0054 stands: the fallback chain still moves only on route failures. Caps are still
     counted per wire model and enforced natively. Every link is still a journalled dispatch.
   - ADR-0050 items 2 to 6 stand: one level of workers, opt-in grants plus `finish`, conditional
     prompt sections, and re-grants between turns.
8. **Changed decisions.**
   - ADR-0050 item 1 (the host appends all four worker tools, "when the `delegation` feature is
     compiled") becomes: the host assembles the members that are enabled, by module id.
   - ADR-0053 items 1 and 7 (a native tool crate composed with constructors, tools appended to
     every main agent) become: one module package per member, each assembled by id over the
     native run service.
9. **The workflow run journal stays its own format.** It gains a version record of its own:
   today's `journal.jsonl` has none, and ADR-0080 left the format unchanged. A binary that does
   not understand the version rejects the journal instead of silently ignoring it.

## Consequences

- One binary serves users with and without delegation. Removing the families is a setting, not
  a rebuild, and the running children and runs it would orphan are kept to their end.
- The per-member grant and the id scoping put the delegation boundary in the host. A compromised
  or buggy tool component can reach only the agents its own operation started.
- The decision component cannot block, wait or reach a worker. Everything slow or effectful
  stays in the native substrate. The price is a snapshot and transition schema that
  `p1-workflow` owns and must version.
- The `delegation` and `workflows` cargo features remain a build option but stop being the user
  mechanism. Tests that asserted feature-off behaviour gain a runtime-disabled twin.

## Alternatives considered

- **The whole workflow as a module (`workflow-implementation` world).** Rejected for the
  interpreter path. It keeps one invocation suspended across `workers.wait` and
  `workflows.wait`, which is the Store re-entry this ADR forbids.
- **One package per family (all four tools together).** Rejected (B-S6-5, D045). A package is
  one `tool` component with one manifest, so the four members would link the union of their
  capabilities, and `worker_result` would get `workers-start`.
- **Keep compile features as the removal mechanism.** Rejected. It conflicts with ADR-0071's
  single binary with modules chosen at run time.

## Evidence

None recorded yet. S6.2 to S6.9 record their DoD rows here, with the PRs, merge commits and
evidence bundle, when this ADR is accepted.
