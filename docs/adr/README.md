# Architecture Decision Records

One file per settled decision: `NNNN-kebab-case-title.md`, numbered densely from
`0001`. The front matter records who decided and where it came from; the body records
the context, the decision, its consequences, the alternatives and the evidence.
`DECISIONS.md` is the frozen ledger of the first slice; these ADRs are the record
from here on.

Run `scripts/adr.py check` (wired into `scripts/gate.sh`) to validate. Read every
ADR through the index below or with `scripts/adr.py list`.

## Index

<!-- adr-index:start -->
| ADR | Title | Status | Date | Deciders |
|---|---|---|---|---|
| ADR-0001 | [p1 is a new project; Iris is a parts donor](0001-p1-is-a-new-project-iris-is-a-parts-donor.md) | accepted | 2026-09-19 | owner |
| ADR-0002 | [One small core with tools and providers as modules](0002-one-small-core-tools-and-providers-as-modules.md) | accepted | 2026-09-19 | owner+lead |
| ADR-0003 | [The harness reshapes itself around the model](0003-harness-reshapes-itself-around-the-model.md) | accepted | 2026-09-19 | owner+lead |
| ADR-0004 | [Compile-time composition with one composition root](0004-compile-time-composition-one-root.md) | accepted | 2026-09-19 | lead |
| ADR-0005 | [Design baseline copied into the repository](0005-design-baseline-copied-into-the-repo.md) | accepted | 2026-09-19 | lead |
| ADR-0006 | [MIT licence and donor provenance](0006-mit-licence-and-donor-provenance.md) | accepted | 2026-09-19 | lead |
| ADR-0007 | [Task state on GitHub Issues and one worktree per task](0007-task-state-on-github-issues-and-worktrees.md) | accepted | 2026-09-19 | lead |
| ADR-0008 | [rustfmt and clippy installed for the gate](0008-rustfmt-and-clippy-for-the-gate.md) | accepted | 2026-09-19 | lead |
| ADR-0009 | [New repository on a task branch with no remotes](0009-new-repo-task-branch-no-remotes.md) | superseded by ADR-0010 | 2026-09-19 | lead |
| ADR-0010 | [Public repository, trunk-based, pushes and merges authorised](0010-public-repo-trunk-based-with-pushes-authorised.md) | accepted | 2026-09-19 | owner |
| ADR-0011 | [The gate is the single definition of green](0011-gate-is-the-single-definition-of-green.md) | accepted | 2026-09-19 | lead |
| ADR-0012 | [Live provider checks are lead-only](0012-live-provider-checks-are-lead-only.md) | accepted | 2026-09-19 | lead |
| ADR-0013 | [One shared cargo target directory across worktrees](0013-shared-cargo-target-dir.md) | superseded by ADR-0014 | 2026-09-19 | owner+lead |
| ADR-0014 | [Per-worktree cargo targets seeded by hardlinks and a global rustc semaphore](0014-per-worktree-targets-and-rustc-semaphore.md) | accepted | 2026-09-20 | lead |
| ADR-0015 | [Send-capable boxed-future contracts, one owner per agent](0015-send-capable-boxed-future-contracts.md) | accepted | 2026-09-20 | lead |
| ADR-0016 | [Flat ordered history and raw tool input validated at the tool](0016-flat-history-and-tool-input-at-the-tool.md) | accepted | 2026-09-20 | lead |
| ADR-0017 | [One terminal stream event and one shared conformance suite](0017-provider-stream-contract-and-conformance-suite.md) | accepted | 2026-09-20 | lead |
| ADR-0018 | [Reasoning replay is opaque and keyed on the configured origin](0018-reasoning-replay-opaque-and-configured-origin.md) | accepted | 2026-09-20 | lead |
| ADR-0019 | [Usage fields kept distinct and unknown reported as unknown](0019-usage-and-cost-reporting.md) | accepted | 2026-09-20 | lead |
| ADR-0020 | [Subscription routes reuse the existing CLI logins](0020-subscription-routes-reuse-cli-logins.md) | accepted | 2026-09-20 | lead |
| ADR-0021 | [The journal is the single truth; in-memory state is its projection](0021-journal-is-the-single-truth.md) | accepted | 2026-09-20 | lead |
| ADR-0022 | [Interrupted calls are reconciled, never re-executed](0022-interrupted-calls-reconciled-not-re-executed.md) | accepted | 2026-09-20 | lead |
| ADR-0023 | [Cancellation precedence and sequential tool execution](0023-cancellation-precedence-and-sequential-tools.md) | accepted | 2026-09-20 | lead |
| ADR-0024 | [Authorization is Permit/Deny at the core; ask lives in the host](0024-authorization-permit-deny-at-the-core.md) | accepted | 2026-09-20 | lead |
| ADR-0025 | [Always-on workspace confinement and read-before-mutate, with apply_patch exempt](0025-workspace-confinement-and-read-before-mutate.md) | accepted | 2026-09-20 | lead |
| ADR-0026 | [Delegation is an optional module, never imposed](0026-delegation-is-optional.md) | superseded by ADR-0050 | 2026-09-20 | owner+lead |
| ADR-0027 | [Workers are dispatched directly, with a machine-wide bounded pool](0027-workers-dispatched-directly-with-a-bounded-pool.md) | accepted | 2026-09-20 | owner+lead |
| ADR-0028 | [Lead authority while the owner is away](0028-lead-authority-while-owner-is-away.md) | accepted | 2026-09-20 | owner |
| ADR-0029 | [Test-first with independent authors and frozen suites](0029-test-first-with-independent-authors-and-frozen-suites.md) | accepted | 2026-09-20 | lead |
| ADR-0030 | [Decisions are recorded as ADRs](0030-decisions-are-recorded-as-adrs.md) | accepted | 2026-09-20 | owner+lead |
| ADR-0031 | [A session file is owned before it is read](0031-a-session-file-is-owned-before-it-is-read.md) | accepted | 2026-09-20 | lead |
| ADR-0032 | [Agents sharing a directory serialize their file mutations](0032-agents-sharing-a-directory-serialize-their-file-mutations.md) | accepted | 2026-09-20 | lead |
| ADR-0033 | [A session resumes only on the route and model that recorded it](0033-a-session-resumes-only-on-the-route-and-model-that-recorded-it.md) | superseded by ADR-0049 | 2026-09-20 | lead |
| ADR-0034 | [Workers are not restored when their parent session resumes](0034-workers-are-not-restored-when-their-parent-session-resumes.md) | accepted | 2026-09-20 | lead |
| ADR-0035 | [The shell tool can run inside a bubblewrap execution boundary](0035-the-shell-tool-can-run-inside-a-bubblewrap-execution-boundary.md) | accepted | 2026-09-20 | lead |
| ADR-0036 | [Context control is a summarizing policy module with a durable, validated replacement](0036-context-control-is-a-summarizing-policy-module-with-a-durable-validated-replacement.md) | accepted | 2026-09-20 | lead |
| ADR-0037 | [Unattended runs end by an observable finish call, with bounded continuation](0037-unattended-runs-end-by-an-observable-finish-call-with-bounded-continuation.md) | accepted | 2026-09-20 | lead |
| ADR-0038 | [Full access is the default; asking is opt-in](0038-full-access-is-the-default-asking-is-opt-in.md) | accepted | 2026-09-20 | owner |
| ADR-0039 | [A provider is composed from a wire adapter, a route and a model profile](0039-a-provider-is-composed-from-a-wire-adapter-a-route-and-a-model-profile.md) | accepted | 2026-09-20 | owner |
| ADR-0040 | [p1 keeps logins in one file keyed by route; environment variables win; other tools' logins are borrowed](0040-p1-keeps-logins-in-one-file-keyed-by-route-environment-variables-win-other-tools-logins-are-borrowed.md) | accepted | 2026-09-20 | owner |
| ADR-0041 | [A headless run waits and continues after a transient provider failure](0041-a-headless-run-waits-and-continues-after-a-transient-provider-failure.md) | accepted | 2026-09-20 | lead |
| ADR-0042 | [A headless run that only summarizes ends as stalled](0042-a-headless-run-that-only-summarizes-ends-as-stalled.md) | accepted | 2026-09-20 | lead |
| ADR-0043 | [TUI: pure state machine in p1-tui, terminal driver in p1-host](0043-tui-pure-state-machine-in-p1-tui-terminal-driver-in-p1-host.md) | accepted | 2026-09-20 | lead |
| ADR-0044 | [p1 login stores pasted API keys in p1's own store](0044-p1-login-stores-pasted-api-keys-in-p1-s-own-store.md) | accepted | 2026-09-20 | owner |
| ADR-0045 | [Research items are issues that end used or discarded; a curator organises research, the lead alone develops](0045-research-items-are-issues-that-end-used-or-discarded-a-curator-organises-research-the-lead-alone-develops.md) | accepted | 2026-09-20 | owner+lead |
| ADR-0046 | [An exhausted account is its own provider error kind; it is never refreshed or retried](0046-an-exhausted-account-is-its-own-provider-error-kind-it-is-never-refreshed-or-retried.md) | accepted | 2026-09-21 | lead |
| ADR-0047 | [The Codex route may speak WebSocket: an adapter-local transport with SSE as the fallback](0047-the-codex-route-may-speak-websocket-an-adapter-local-transport-with-sse-as-the-fallback.md) | accepted | 2026-09-21 | owner+lead |
| ADR-0048 | [Providers may tell the operator something: a display-only notice event](0048-providers-may-tell-the-operator-something-a-display-only-notice-event.md) | accepted | 2026-09-21 | lead |
| ADR-0049 | [Model selection and switching a session to another model](0049-model-selection-and-switching-a-session-to-another-model.md) | accepted | 2026-09-21 | owner+lead |
| ADR-0050 | [Every main agent can start workers; a worker gets exactly the tools its parent grants](0050-every-main-agent-can-start-workers-a-worker-gets-exactly-the-tools-its-parent-grants.md) | accepted | 2026-09-22 | owner+lead |
| ADR-0051 | [A worker without a command tool may finish done; the result says it was not verified](0051-a-worker-without-a-command-tool-may-finish-done-the-result-says-it-was-not-verified.md) | accepted | 2026-09-23 | owner+lead |
| ADR-0052 | [Usage ledger: p1-usage module and p1 usage command](0052-usage-ledger-p1-usage-module-and-p1-usage-command.md) | proposed | 2026-09-23 | lead |
| ADR-0053 | [Workflows are an optional module: a sandboxed script orchestrates workers under roles and caps](0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md) | accepted | 2026-09-23 | owner+lead |
| ADR-0054 | [Workflow roles have a fallback chain for route failures; DeepSeek is the shipped worker](0054-workflow-roles-have-a-fallback-chain-for-route-failures-deepseek-is-the-shipped-worker.md) | proposed | 2026-09-23 | owner+lead |
| ADR-0055 | [A successful command that changes the workspace counts as progress for the stall guard](0055-a-successful-command-that-changes-the-workspace-counts-as-progress-for-the-stall-guard.md) | accepted | 2026-09-23 | lead |
| ADR-0056 | [The TUI follows the SLAB Harness design system and its implementation handoff](0056-the-tui-follows-the-slab-harness-design-system-and-its-implementation-handoff.md) | proposed | 2026-09-23 | owner+lead |
<!-- adr-index:end -->

## Writing one

1. `scripts/adr.py new "Short decision title" [--deciders owner|lead|owner+lead] [--supersedes N …]`.
   The file is created with status `proposed`, today's date and the template's sections.
2. Fill every section; delete the `<…>` placeholders. An owner decision says so in
   Context and quotes the owner's words when a source has them. If no alternative or
   evidence is recorded, write `None recorded.` — never invent one.
3. Keep the status `proposed` until the change is merged; then set it to `accepted`
   (or `rejected`).
4. Never edit an accepted ADR except its `status` and `superseded_by`. To reverse a
   decision, write a NEW ADR with `--supersedes N`; the old one becomes `superseded`.
5. `scripts/adr.py index` to regenerate the table below, then `scripts/adr.py check`.
