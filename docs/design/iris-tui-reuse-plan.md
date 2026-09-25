# Iris TUI reuse — workflow implementation plan

Planning issue: [#91](https://github.com/5omeOtherGuy/phaseone/issues/91).
Status: proposed execution plan; no migration workflow launched.
Owner requirement: reuse applicable Iris implementations and tests, while
strictly preserving p1 modularity and the interactive owner TUI.

Donor: `5omeOtherGuy/iris-agent` at
`5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`. Do not silently follow its main branch.
Both projects use Ratatui 0.30. Inspection has established concrete candidates,
not verified that every donor module is correct or directly transplantable.

## 1. Scope and completion rule

Inventory EVERY file under donor `src/ui/`, plus dependencies reached outside it,
and give each reusable unit a disposition:

- **Port**: copy implementation and relevant tests, minimally adapting contracts.
- **Already equivalent**: prove p1 already satisfies the donor behavior with tests.
- **Adapt**: reuse a mechanism behind a p1-owned interface, not Iris's runtime.
- **Blocked**: name the missing dependency approval, consumer or product decision.
- **Exclude**: show why it is Iris-only, duplicates an accepted p1 implementation,
  or conflicts with the selected terminal surface/design.

Each inventory row records donor commit/path/symbol, license, dependencies,
destination, real consumer, issue, acceptance tests and final disposition.
No unexplained omissions and no implementation claim for a blocked/deferred row.
Before calling the overall migration complete, independently review exclusions
and resolve blockers or explicitly narrow the owner-approved scope.

The inventory is grouped below for planning; these are provisional dispositions,
not a substitute for the per-file audit.

| Donor area (paths relative to `src/ui/`) | Intended reuse / boundary |
|---|---|
| `textengine.rs`, `text.rs`, `tui/{text,wrap}.rs` | Port grapheme-safe width, fit, sanitization and wrapping helpers/tests into p1-tui. Adapt zero-width, oversized-cluster, indentation and control-byte policies to p1; don't inherit incompatible edge behavior blindly. |
| `selector.rs`, `slash.rs`, `picker.rs`, `modal.rs`, `tui/{component,overlay}.rs` | Port selection/windowing/focus/key-routing mechanics. Keep p1 command definitions and ordinary explicit composition; do not introduce a component registry or service bag. |
| `tui/screen.rs` editor helpers and `ratatui-textarea` integration | Adapt multiline editing, paste, cursor, history/undo and bounded composer behavior. Candidate dependency requires approval if not already in workspace. Keep SLAB chrome/key contracts. |
| `tui/pager.rs`, `tui/{transcript,rows,pane,panel}.rs` | Port applicable scroll/follow/reveal, viewport, caching, folding and hit-testing mechanisms. Bind to p1 transcript IDs and tool-neutral presentation; preserve p1 transcript retention and authoritative event ordering. |
| `markdown.rs`, `highlight.rs`, `hyperlink.rs` | Audit/port rendering algorithms needed by current transcript/output consumers. Theme supplied explicitly; parser/highlighter dependencies require approval. Host owns link activation. No embedded terminal control bytes in display text. |
| `tui/streaming/{chunking,collector,controller,escapement,table_holdback,mod}.rs` | Port stable-prefix/tail collection, Markdown boundary handling and pacing with tests. Rendering is never canonical agent history. Keep reduced-motion behavior and original event identity/order. |
| `tui_loop.rs`, `harness_actor.rs`, `steering.rs` | Adapt render scheduling, resize debounce, responsive input, cancellation/approval precedence and safe-boundary commands in p1-host. Reuse existing p1 pump where equivalent; do not transplant Wayland Harness ownership. |
| `tui.rs`, `tui/pager.rs` lifecycle, `screen_mode.rs`, `terminal_surface.rs` | Audit lifecycle/restore/synchronized-update helpers for p1's existing surface. Do not install Iris's second inline renderer merely to reuse code; record excluded backend-specific parts and reuse applicable tests. |
| `clipboard.rs`, `terminal_env.rs`, `terminal_doctor.rs`, `zwj_probe.rs` | Audit capability, paste/copy and diagnostic helpers. Terminal probing, environment reads, subprocesses and clipboard I/O belong in host; pass results into pure rendering. Unsupported capabilities need explicit safe fallback. |
| `tui/{activity,frame_stats}.rs` | Reuse activity timing, cache instrumentation and test hooks where a current consumer needs them. No new telemetry service, provider accounting claims or background subsystem. |
| `delegation_dashboard.rs`, `tui/{tool_render,shell_command}.rs` | Reuse view/layout mechanics with p1 worker snapshots and tool presentation contracts. No Iris delegation executor or tool-name dispatch in the renderer. Coordinate with #89. |
| `ask_user_question.rs`, `login.rs`, `settings_menu.rs`, `task_view.rs` | Extract reusable forms, navigation and pending/read-only state. Real effects remain host-owned and require existing contracts; don't implement authentication/task engines as part of UI reuse. |
| `tui/session_menu/{mod,git_menu,jj_menu,tree_menu}.rs` | Reuse generic menu/windowing and presentation where p1 has a consumer. VCS/filesystem discovery stays host-owned. Iris-only actions without a p1 service are blocked/excluded explicitly, never fake controls. |
| `palette.rs`, `theme.rs`, `symbols.rs`, `tui/startup.rs`, `mod.rs` | Keep p1 SLAB tokens, home behavior and exports. Extract compatible capability/accessibility/layout mechanisms, not Iris branding or its theme framework. Audit wiring for otherwise missed helpers. |

## 2. Non-negotiable modularity gates

1. `p1-core` remains dependent only on contracts: no terminal, storage, UI,
   concrete provider/tool, dashboard or donor runtime dependency.
2. `p1-tui` owns deterministic presentation state/rendering and input-to-command
   translation. New pure components receive values, viewport and injected time;
   no agent ownership, auth/VCS access, filesystem reads, terminal probes or
   background work. Do not expand existing runtime support into a second host.
3. `p1-host` owns terminal lifetime, async orchestration and adapters to existing
   p1 services. Keep one owner of mutable agent state and cancellation semantics.
   Public async contracts remain Send-capable; donor Rc/RefCell patterns must not
   leak across these seams.
4. Tools/providers never import terminal types. Tool presentation stays neutral;
   renderers do not execute tools or recover semantics by matching tool names.
5. Use ordinary constructors and narrow typed interfaces inside the native TUI (the TUI
   stays native under ADR-0071). No global registry, hidden singleton settings, service
   locator or shared mutable cross-module state. Pass capability/theme data explicitly.
6. Reuse implementation, not Iris's Nexus/Wayland/Mimir dependency graph. Keep
   private helpers as modules; create a crate only for a real independently
   selectable boundary/consumer, not one crate per donor file.
7. Preserve ADR-0043, ADR-0056 and `seams.md`. Any actual boundary/decision change
   needs a proposed superseding ADR first; do not rewrite accepted ADRs.
8. For each port, verify the resolved dependency graph, an allowed-dependency
   regression test and behavior tests at the seam. Core isolation alone cannot
   detect all forbidden p1-tui dependencies. Do not rely only on text grep.
9. Keep SLAB's frozen cell/style oracles, approvals, workers and line frontend.
   Missing/unknown usage stays absent, not zero. No private brain/board data or
   storage migration enters this programme.

## 3. Ordered increments and dependency graph

**P0 — Baseline, inventory and contracts.**
Preserve the unmerged #89 slice; settle its independent review/landing before
branching migrations from an accepted base, or record an explicit stacked base.
Capture synthetic reference behavior and frame/work counters. Audit the complete
donor manifest, licenses, dependency availability and p1 equivalences.
Split #91 into independently claimable implementation issues with exact owned
paths and acceptance cases. New crates not already in the workspace need approval;
blocked dependency proposals are owner-decision issues, not silent substitutions.

**P1 — Text foundations and selection primitives.**
Two independent patch-authoring lanes: text safety and selectors. Import donor
tests and independently authored p1 edge cases before wiring replacements.
Lead serializes shared exports, manifest and lockfile edits.

**P2 — Host responsiveness and terminal safety.**
Port the scheduler/resize policy and useful restore/capability handling in host.
Audit the existing p1 pump against Iris's always-live-input cases; adapt only gaps.
This lane may overlap pure P1 authorship, not conflicting host integration/builds.

**P3 — Composer, overlays and navigation.**
Depends on P1 and relevant P2 capabilities. Adopt editor mechanics, selection,
focus, search/menu windowing and safe paste; preserve p1 command meanings.
No Iris settings/auth/task mutation machinery.

**P4 — Transcript, scrolling, Markdown and streaming.**
Depends on P1 and P2. Sequential sub-increments:
scroll/fold/reveal and frame geometry; approved Markdown/highlighting/link support;
then stable-prefix/tail streaming, pacing and cache integration.
Do not parallelize edits to shared transcript/state/render files.

**P5 — Dashboard and host-view adapters.**
Depends on P3/P4 and #89's accepted composition seam. Apply applicable worker,
activity, menu and form mechanisms to real p1 consumers. Quota still comes from
the existing host snapshot adapter. Brain/board read-only contracts stay with
their leads; UI migration does not authorize new project actions.

**P6 — Full integration, visual acceptance and inventory closure.**
Remove replaced duplicate implementations only after consumer migration/tests.
Reconcile every donor inventory row, dependency/license entry and test result.
Run independent integration review and the full gate. At the explicit visual
milestone, Kimi K3 inspects at least two real-terminal capture/fix rounds, followed
by lead inspection against SLAB. Validate the built artifact in a separate
synthetic session; do not restart/replace the owner's current process.

Dependency sketch:

```
P0 -> P1(text || selectors) ----> P3 ----+
  \-> P2(host/lifecycle) -------> P4 ----+-> P5 -> P6
                    P1 also -> P4
```

## 4. Workflow orchestration

Use p1 workflows, not recursive/direct worker delegation. Roles in scripts are
`worker`, `reviewer`, `verifier`, `judge`; model/route selection is host settings.
Preflight grants, actual routes, caps and no-fallback configuration before work.
Implementation uses the authorized Bunny route. MiMo direct p1 dispatch remains
blocked; no identity spoofing. Kimi is reserved for P6, not routine implementation.

Each increment follows this evidence pipeline:

1. **Verifier authors acceptance cases** from contracts/donor behavior, independent
   of the implementation. Lead reviews and freezes them; retain existing oracles.
2. **Worker authors a bounded patch** from the pinned donor, including provenance,
   dependencies, migrated tests and explained adaptations. Public source and
   synthetic data only. No recursive workers or private integration access.
3. **Lead applies/integrates** with apply_patch. The Bunny function-only route
   cannot receive GPT's freeform patch tool: grant read-only tools and require
   returned patch text. Do not work around this with shell writes.
4. **Reviewer inspects the actual applied diff** against the donor and modularity
   checklist, not the author's summary. Findings name paths, behavior and severity.
5. **Verifier confirms/refutes findings** with concrete cases. Lead applies
   repairs and reruns affected tests; changed repairs receive renewed review.
6. **Judge reconciles evidence**; lead independently inspects and runs acceptance.
   A model's done/exit-zero verdict is not permission to land.

Parallelize only disjoint paths and independent read-only analysis. One writable
task worktree per increment, no two editing agents in a checkout. Worktree
provisioning must occur through authorized tooling/workspace roots; do not bypass
the current patch confinement to create files outside this workspace.
No worker edits STATUS.md, manifests, lockfiles or shared integration files.

The lead-application step is a deliberate orchestration barrier: collect returned
patches, end the authoring phase, apply and inspect, then review the resulting
revision. Use `resume_from` only for an unchanged prefix and preserved artifacts;
changed code invalidates old review evidence. Never let reviewers race unapplied
patches. Preserve dispatch/cap accounting across retries; do not split runs to
evade a cap. A capped, failed or blocked envelope stops dependent work.

Inspect step status/value/evidence and run failed/blocked/capped/not_verified/
fell_back counts. `Completed` alone is not acceptance. Record actual route/model,
repairs, tool/provider failures, duration and independently accepted results.
Unknown HTTP retries, refreshes, cash or quota usage remain unknown.

## 5. Required acceptance matrix

- **Text:** CJK, combining marks, emoji/ZWJ/flags, tabs and control sequences,
  cross-span boundaries, zero/one-cell widths, too-wide clusters and guaranteed
  forward progress; measured heights match rendered rows.
- **Input:** typing/paste/navigation during streaming, tool execution, parked
  approval and compaction; focus precedence, Escape/Ctrl-C, no accidental command
  submission or dropped/duplicated input. No sleeps: fake time/explicit signals.
- **Composer/menus:** multiline cursor, undo/redo, Unicode edits, bounded large
  paste, empty/filtering/wrapping/clamped lists, resize and stable selection IDs.
- **Transcript:** anchored scroll never jumps on append/finalize; follow/reveal,
  fold/search and resize semantics; no lost final partial line, duplicated content
  or incorrect Markdown table/fence commits. Cancellation preserves visible truth.
- **Terminal:** normal/error/panic restoration and idempotence, synchronized-update
  closure, unsupported capability fallback and safe clipboard/link behavior.
  Byte fixtures must not probe a real terminal or user account.
- **Geometry/style:** existing SLAB text/style snapshots plus 80x24, 120x40,
  zero/tiny/offset bounds, rapid resize, wide panes and color/reduced-motion modes.
- **Performance:** long-transcript and burst-stream fixtures, cache invalidation
  correctness and deterministic render-work counters. No flaky wall-clock limits
  or invented speedup targets; report measured baseline/change separately.
- **Architecture:** allowed dependency graph, no UI in core/tools/providers,
  synthetic data ownership, actual explicit composition and unchanged public APIs.

Lead runs focused locked/offline tests during development, then `scripts/gate.sh`
before each landing and on the final integrated revision. Immediately before every
build require >=12,884,901,888 available bytes; use CARGO_BUILD_JOBS=2 and the
8,589,934,592-byte process-group stop guard. Log admission, sampled minimum, elapsed
time and exit status. Keep per-worktree targets; don't share workspace artifacts.
Serialize builds to conserve disk; a disk abort is not a source/test pass.

## 6. Landing and deliverables

Deliver the per-symbol disposition manifest, small donor-attributed ports, migrated
and independent tests, dependency-approval/ADR records where needed, and full
workflow/verification evidence. Preserve MIT notices and Apache-2.0 headers,
license and NOTICE entries for Codex-derived streaming code; record modifications.

Each increment must be green AND independently reviewed before merging, per this
assignment's stricter rule. Commit explicit paths; record donor paths/hash in commit
messages. Check CI for the landed revision. A failed integrated gate holds landing.
Rollback is a revert of the bounded increment, not weakening tests or altering the
owner's active binary. The programme is complete only after P6 and reconciled scope,
not when the last authoring worker returns.
