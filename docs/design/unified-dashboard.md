# Unified terminal dashboard — first increment

Issue [#89](https://github.com/5omeOtherGuy/phaseone/issues/89).
Base: `e95337d`. Proposed architecture: ADR-0064. This is scoped owner-authorized
work; unrelated halted observability and owner-decisions TUI WIP are not included.

## Shipped inventory

| Surface | Implementation | Proven boundary / limitation |
| --- | --- | --- |
| Agent home and transcript | `p1-tui`, `p1-host/src/tui.rs` | Pure state/cells versus host-owned async terminal lifecycle (ADR-0043); SLAB oracle (ADR-0056). |
| Ledger and tool output panes | `p1-tui/src/render/{ledger,output}.rs` | Data supplied by Screen/host; not a quota dashboard. |
| Worker activity pane | `p1-tui/src/render/workers.rs`, host worker projection | Session-local worker data, attention ordering, unknown cost; not durable fleet observability. |
| Diff | `p1-tui/src/render/diff.rs` | Renderer/fixtures exist; generally unavailable mode until host diff seam (#69). |
| Quota ledger | `p1 usage`, `p1-host/src/usage.rs`, `p1-usage` | Snapshot/probe separation, pure palette-independent renderer, plain/live watch. Separate terminal lifecycle today. |
| Line frontend | `p1-host` FrontEnd | Preserved; unified dashboard does not make TUI mandatory. |

Existing tests prove individual renderers and fixtures, not unified navigation or
live brain/board integration. The base has the verified startup deadlock fix.

## Reviewable increments

1. Bare pure shell and explicit view interface, existing worker renderer as a
   read-only adapter, synthetic preview and navigation/viewport tests. Initially
   a module inside `p1-tui`, not a second terminal driver. No live agent-TUI changes.
2. Quota adapter: host converts existing `p1-usage` rendered snapshot spans to
   UI lines; no probe/auth dependency enters the pure framework. Reuse used-share,
   stale-data and unknown-value semantics. Compose quota and worker views in one
   host-owned terminal lifecycle; preserve agent input, approvals and cancellation.
   Decide application navigation keys with the concrete composition, not here.
3. Extract independently selectable framework/view crates when real composition
   proves the boundary. Ordinary constructors only; no global registry/plugin
   loader. No UI, storage or project ownership enters `p1-core`.
4. Concrete Kimi visual design brief only after a renderable shell: at least two
   real-terminal capture → inspect → fix rounds, then lead inspection. Existing
   SLAB mocks remain authoritative; no redesign accepted from a model finish.
5. Brain/board: agree read-only versioned snapshot contracts with their leads
   before adapters. Each owns source/data; use synthetic fixtures. Show source,
   timestamp, availability/staleness and absent values explicitly. No action/write
   API, storage migration or free-route access to private data.

## Verification and tracking

- Tests: no/one/multiple views, wrapping navigation, replacing/removing supplied
  views, 80x24 and 120x40 buffers, zero/tiny viewport, worker ordering and unknown
  cost, no misleading unwired action hints; preserve original pane snapshots.
- Lead reviews actual diff and runs focused tests, then `scripts/gate.sh` before
  merge. Rust admission >=12 GiB, floor >=8 GiB, `CARGO_BUILD_JOBS=2`.
- #89 owns this slice and follow-up composition; #68/#69 retain existing host
  gaps. Existing #63 tracks unavailable attempt-level telemetry. Do not restart
  the halted observability programme just to populate dashboard fields.
- Routine implementation uses p1 role workflows on the configured free worker;
  record actual dispatch/envelope and independent acceptance separately.
- Secure operator/route/model/quota evidence stays in the owner-designated trial
  directory, never in public fixtures. Unknown cash/quota/retries stay unknown.

## First-slice checkpoint

The pure shell and read-only worker adapter are implemented under
`crates/p1-tui/src/dashboard*`. `dashboard_preview` is an offline synthetic
TestBackend example, not a second terminal application:

```
cargo run -p p1-tui --example dashboard_preview
cargo run -p p1-tui --example dashboard_preview -- wide next
```

The shell deliberately clips from the top left; view-specific scrolling and live
navigation bindings are not implemented in this slice. Existing live worker-pane
rendering and controls remain unchanged.

Lead verification: 233 tests in all 14 freshly built p1-tui test binaries passed,
including five new dashboard tests; both preview dimensions/content passed.
After disk recovery, the full locked workspace gate **passed** offline with
jobs=2 on 2026-09-24 (148.027 seconds, exit 0). Minimum sampled free space was
15,404,658,688 bytes, above the 8 GiB stop floor. This supersedes the earlier
disk-floor termination. No merge or visual-design acceptance is claimed;
final acceptance remains required before landing.

Final independent source review (wf7, DeepSeek V4.1 Flash on the primary Go
subscription route) completed with no source-review blocker or fallback. Lead
inspection confirms the composition boundary and preservation of the live worker
footer/focus behavior. The review ran source/ADR checks only, not Rust tests;
its workflow verification flag does not establish runtime or merge acceptance.

The owner requires immediate fixes through separate cheap/free workers, not a
deferred low-priority backlog. All three findings below are now in implementation
workflow wf8 on the configured free worker role, with no fallback. It authors
a patch for lead application/review; completion still requires runtime checks.
The first proposal was rejected before application: lead inspection found
incorrect partial-wide-character padding, conflicting existing filler assertions,
an incorrect character-position assertion and string-method calls on Line.
The same free worker is repairing all of these; no finding is considered closed.

- Existing body lines retain their styles and are not padded; only missing rows
  and the navigation strip explicitly fill BLOCK. Fix the shell to give complete
  body rows a default BLOCK background while preserving explicit view styles,
  and assert short/empty-row backgrounds and style precedence in TestBackend.
- The dashboard worker-adapter tests cover 80/120 columns, not the `<56` compact
  branch or its tiny-width boundary. Existing renderer tests do not substitute
  for adapter-specific coverage.
- Top-left anchoring intentionally forces left alignment even on fitting lines.
  The current test asserts the clipped case; add the fitting-line case to make
  this policy explicit.

These are retained findings, not newly passing tests or a claim that the separate
right-pane regression (#92) was reviewed by wf7. Runtime verification of that
fix remains pending disk recovery; no source changes were made for this review.

## Ownership handoff — 2026-09-24

- This worktree's lead owns the dashboard shell (#89) and the right-pane
  selection regression (#92). Existing unmerged dashboard work is retained.
- The separate interactive Astra in pane `%44` owns Iris TUI migration (#91).
  The owner supplies a read-only copy of the proposed Iris plan. This lead will
  neither revise that plan nor dispatch migration workers.
- Coordinate before overlapping `p1-tui/src/state.rs`, the worker renderer, UI
  exports or host terminal composition. The pane fix belongs here; donor
  migrations belong in the other worktree. No shared target directory, source
  overwrite or restart/replacement of the owner's interactive TUI.
- The current shell remains pure and caller-composed; worker data is read-only.
  Future quota/service adapters must preserve the host/UI boundary described
  above. Iris handoff does not authorize live integration or relax SLAB tests.
- Dashboard source review can proceed during disk recovery. Runtime acceptance
  waits for safe space recovery and guarded checks; independent worker
  supervision is nonblocking by default.
