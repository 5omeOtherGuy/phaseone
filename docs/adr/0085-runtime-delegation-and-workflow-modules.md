---
adr: 85
title: Runtime delegation and workflow modules
status: accepted
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [epic #206, DECISIONS.md D22, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0050-every-main-agent-can-start-workers-a-worker-gets-exactly-the-tools-its-parent-grants.md, docs/adr/0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md, docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, docs/adr/0054-workflow-roles-have-a-fallback-chain-for-route-failures-deepseek-is-the-shipped-worker.md, docs/adr/0081-native-foundation-and-runtime-components.md, docs/adr/0082-component-abi-and-execution-ownership.md, docs/design/modules/wit.md, modules/wit/delegation.wit, modules/wit/worlds.wit, docs/design/delegation.md, docs/design/workflows.md]
---
# ADR-0085: Runtime delegation and workflow modules

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

Every S6 slice, with its merge commit on main and the main `gate` run of that commit:

- S6.1, PR #231, merge commit `e850cb88` (`e850cb88ba4e9f7902b185b09ec04106333e9e67`): items 1, 2 and 8
  on the host side — the delegation and child assembly leaves `run.rs` for
  `crates/p1-host/src/catalog/` (`delegation.rs`, `children.rs`, `workflow.rs`, `worktree.rs`), so a
  member can later be assembled by id, and `WORKER_MODULES` and `WORKFLOW_MODULES` stay the one
  definition of each family's members; a pure move, no behaviour change. main gate run 36176937286,
  success.
- S6.0.1, PR #242, merge commit `066456a1` (`066456a1d6d0717a4fd89dd602e9f495d630b11f`): the flaky
  case of `crates/p1-host/tests/worker_usage.rs` — a second worker's session file is present after a
  clean exit, so the run records of item 1's worker service are trustworthy; it delivers no Decision
  item. main gate run 36187084439, success.
- S6.0.2, PR #239, merge commit `28b1878f` (`28b1878f23718ab0323d0f5e9bbc2a1fcb709932`): the flaky
  spawn of `crates/p1-hook-shadow/tests/hook.rs`, where the fake binary is created by a waited-for
  child so no fork inherits a write descriptor; it delivers no Decision item. main gate run
  36193331319, success.
- S6.6, PR #288, merge commit `105210c9` (`105210c99c7b2171e24d53269e1ccb47c14120b4`): this ADR,
  drafted and merged `proposed`. main gate run 36219277723, success.
- S6.2, PR #277, merge commit `bb8fd798` (`bb8fd798bf15ece4a426ebfb18c3b55bf9d2a74d`): items 3 and 4 —
  `crates/p1-workers/src/scope.rs` splits the worker operations into `WorkersStart`, `WorkersObserve`
  and `WorkersControl` and scopes every worker id to the scope that started it, with the
  `worker_boundary` suite over the loaded members; its own main gate run 36220218355 was cancelled by
  the concurrency group and is covered by the descendant `8cbefd7a` run 36220311029, success.
- S6.3, PR #282, merge commit `ffe4a5fd` (`ffe4a5fd4ca2f87950e8516303a590e93d040213`): item 5 — the
  decision logic moves to `crates/p1-workflow/src/decision/` as `plan-step` and `accept-step` over the
  native engine, which keeps run state, counters, ordering and caps, with the `workflow_boundary`
  suite; its own main gate run 36221894086 was cancelled by the concurrency group and is covered by
  the descendant `28f851f9` run 36222043025, success.
- S6.4.2, PR #283, merge commit `28f851f9` (`28f851f9e46f99dce0fb9c2b7e80e92691ee31e8`): item 2 — the
  eight member packages `modules/p1-module-worker-{start,result,continue,cancel}` and
  `modules/p1-module-workflow-{start,status,result,cancel}`, each one `tool` component over its own
  interface. main gate run 36222043025, success.
- S6.4.1, PR #305, merge commit `8186c5a0` (`8186c5a00772713287d01009b47f1eebe64263a7`): items 2 and 3 —
  each native member is built from its own trait (`p1-tool-delegate`, `p1-tool-workflow`), so
  `worker_result` gets observe and never start. main gate run 36227008515, success.
- S6.5, PR #302, merge commit `051642e9` (`051642e922a8d20fc65fa473dd092748c5e7b07a`): items 3 and 4
  measured — `crates/p1-host/tests/worker_grants.rs` and `worker_add_tools.rs` pin that a member
  reaches only the children its own operation started, and that a re-grant reassembles the child
  between turns; its own main gate run 36228374787 was cancelled by the concurrency group and is
  covered by the descendant `7fe39b04` run 36228482257, success.
- S6.7.1, PR #319, merge commit `e56369e7` (`e56369e7196e9ec7db9d768da87d15f9d539dc91`): items 3, 4 and
  5 — `crates/p1-module-runtime/src/delegation.rs` links `workers-start`, `workers-observe`,
  `workers-control` and `workflows` over the native traits, one optional service per interface and
  `MissingService` when a granted interface has none. main gate run 36244662754, success.
- S6.7.2, PR #325, merge commit `5e014ec8` (`5e014ec821c532f03ae0395b28aae6bf5ca9080e`): items 2 and
  4 — the host assembles each member by its module id, the module-services hook takes the package
  name, `ToolServices` carries the parent agent and one worker scope is keyed per generation, family
  and parent; its own main gate run 36246282289 was cancelled by the concurrency group and is covered
  by the descendant `526347bd` run 36247378318, success.
- S6.9, PR #342, merge commit `c1da83f7` (`c1da83f7a3bdc07a2330ccb46c0b4ff52c344277`): items 5 and 9 —
  the loaded `workflow-decision` component (`modules/p1-module-workflow-decision/`,
  `crates/p1-module-runtime/src/workflow_decision.rs`), one synchronous instance per call over the
  snapshot and transition schema `p1-workflow` owns, and the run journal's own version record, which
  `read_journal` and resume refuse when it is unknown or newer; its own main gate run 36246626019 was
  cancelled by the concurrency group and is covered by the descendant `526347bd` run 36247378318,
  success.
- S6.8, PR #351, merge commit `53daa7bf` (`53daa7bf4e7ad374eb49716507105e841b9c5a83`): item 6 —
  disabling the workers or the workflow capability removes those tools and their prompt sections from
  the next assembly, while children and runs already started keep their generation, and the disabled
  path is an explicit error, never an implicit fallback
  (`crates/p1-module-tests/tests/capability_removal.rs`). main gate run 36264310348, success.
- Acceptance: this ADR is accepted with the recorded S6 verdicts — the Fable judge's ACCEPT on every
  slice listed above (S6.1, S6.0.1, S6.0.2, S6.2, S6.3, S6.4.1, S6.4.2, S6.5, S6.6, S6.7.1, S6.7.2,
  S6.8 and S6.9).
- Known follow-up: under D083b (B-S6-11), shipped environments still take the native worker and
  workflow members until #355 (S3.8.0) lands; issue #363 (S6.11) turns the eight member packages into
  official-release host entries, with no `modules.lock` entries.
