# Architecture Decision Records

One file per settled decision: `NNNN-kebab-case-title.md`, numbered from `0001`; a
number is unique and reserved on the board before the file is written, and pull requests
merge in any order, so a number not merged yet leaves a gap (ADR-0107). The front matter records who decided and where it came from; the body records
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
| ADR-0004 | [Compile-time composition with one composition root](0004-compile-time-composition-one-root.md) | superseded by ADR-0071 | 2026-09-19 | lead |
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
| ADR-0035 | [The shell tool can run inside a bubblewrap execution boundary](0035-the-shell-tool-can-run-inside-a-bubblewrap-execution-boundary.md) | superseded by ADR-0096 | 2026-09-20 | lead |
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
| ADR-0052 | [Usage ledger: p1-usage module and p1 usage command](0052-usage-ledger-p1-usage-module-and-p1-usage-command.md) | accepted | 2026-09-23 | lead |
| ADR-0053 | [Workflows are an optional module: a sandboxed script orchestrates workers under roles and caps](0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md) | accepted | 2026-09-23 | owner+lead |
| ADR-0054 | [Workflow roles have a fallback chain for route failures; DeepSeek is the shipped worker](0054-workflow-roles-have-a-fallback-chain-for-route-failures-deepseek-is-the-shipped-worker.md) | accepted | 2026-09-23 | owner+lead |
| ADR-0055 | [A successful command that changes the workspace counts as progress for the stall guard](0055-a-successful-command-that-changes-the-workspace-counts-as-progress-for-the-stall-guard.md) | accepted | 2026-09-23 | lead |
| ADR-0056 | [The TUI follows the SLAB Harness design system and its implementation handoff](0056-the-tui-follows-the-slab-harness-design-system-and-its-implementation-handoff.md) | accepted | 2026-09-23 | owner+lead |
| ADR-0057 | [A tool describes each call's target; the host and the UI stop matching tool names](0057-a-tool-describes-each-call-s-target-the-host-and-the-ui-stop-matching-tool-names.md) | accepted | 2026-09-23 | lead |
| ADR-0058 | [p1 spawns the brain shadow hook, detached and fail-open, as an optional module](0058-p1-spawns-the-brain-shadow-hook-detached-and-fail-open-as-an-optional-module.md) | accepted | 2026-09-23 | owner+lead |
| ADR-0059 | [A tool describes its results and its destructiveness; the host describer keeps no tool-name table](0059-a-tool-describes-its-results-and-its-destructiveness-the-host-describer-keeps-no-tool-name-table.md) | accepted | 2026-09-23 | lead |
| ADR-0060 | [Per-task SSD Cargo targets and the machine-wide rustc limit](0060-one-shared-cargo-target-per-repository-across-worktrees-rustc-serial-ignores-slots-whose-holder-is-stopped.md) | superseded by ADR-0119 | 2026-09-23 | owner+lead |
| ADR-0061 | [A route may be self-contained: store-only credentials never read another tool's login](0061-a-route-may-be-self-contained-store-only-credentials-never-read-another-tool-s-login.md) | superseded by ADR-0074 | 2026-09-24 | owner |
| ADR-0062 | [A plan refusal is its own provider error kind; it is never refreshed or retried](0062-a-plan-refusal-is-its-own-provider-error-kind-it-is-never-refreshed-or-retried.md) | accepted | 2026-09-24 | lead |
| ADR-0063 | [Claude requests its native 1M context window through a named long_context route setting](0063-claude-requests-its-native-1m-context-window-through-a-named-long-context-route-setting.md) | accepted | 2026-09-24 | lead |
| ADR-0064 | [Unified dashboard uses a pure shell and explicit view composition](0064-unified-dashboard-uses-a-pure-shell-and-explicit-view-composition.md) | accepted | 2026-09-24 | owner+lead |
| ADR-0065 | [p1 installs from its release channel into a prefix and the binary names its commit](0065-p1-installs-from-its-release-channel-into-a-prefix-and-the-binary-names-its-commit.md) | accepted | 2026-09-24 | lead |
| ADR-0066 | [p1 builds and verifies on GitHub Actions; local cargo only for check and the lead's deployed binary](0066-p1-builds-and-verifies-on-github-actions-local-cargo-only-for-check-and-the-lead-s-deployed-binary.md) | superseded by ADR-0107 | 2026-09-24 | lead |
| ADR-0067 | [Zen free routes present the OpenCode client identity](0067-zen-free-routes-present-the-opencode-client-identity.md) | accepted | 2026-09-24 | owner+lead |
| ADR-0068 | [Tool output is masked for credential shapes before history, journal and summaries](0068-tool-output-is-masked-for-credential-shapes-before-history-journal-and-summaries.md) | accepted | 2026-09-25 | lead |
| ADR-0069 | [Provider reads are bounded inside the connection: first byte 120 s, stream idle 300 s](0069-provider-reads-are-bounded-inside-the-connection-first-byte-120-s-stream-idle-300-s.md) | accepted | 2026-09-25 | lead |
| ADR-0070 | [A route may declare no credential for a proxy that injects it](0070-a-route-may-declare-no-credential-for-a-proxy-that-injects-it.md) | accepted | 2026-09-25 | lead |
| ADR-0071 | [p1 migrates to WebAssembly modules: native core and host load tools, providers and policies by name](0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md) | accepted | 2026-09-25 | owner+lead |
| ADR-0072 | [A workflow step that ends without finish gets one repair turn](0072-a-workflow-step-that-ends-without-finish-gets-one-repair-turn.md) | accepted | 2026-09-25 | lead |
| ADR-0073 | [Workflow steps can run in their own git worktree](0073-workflow-steps-can-run-in-their-own-git-worktree.md) | accepted | 2026-09-25 | lead |
| ADR-0074 | [A second Claude subscription route: claude-code-oauth borrows a named login directory and p1 login imports a login](0074-a-second-claude-subscription-route-claude-code-oauth-borrows-a-named-login-directory-and-p1-login-imports-a-login.md) | accepted | 2026-09-25 | owner |
| ADR-0075 | [The TUI shows workflow runs as a live tree through a structured FrontEnd seam](0075-the-tui-shows-workflow-runs-as-a-live-tree-through-a-structured-frontend-seam.md) | accepted | 2026-09-25 | owner+lead |
| ADR-0076 | [Manual compaction: /compact in the TUI and --compact on resume](0076-manual-compaction-compact-in-the-tui-and-compact-on-resume.md) | accepted | 2026-09-25 | owner+lead |
| ADR-0077 | [Builds on the stream boxes](0077-builds-on-the-stream-boxes.md) | superseded by ADR-0097, ADR-0105 | 2026-09-25 | owner+lead |
| ADR-0078 | [Connection resources and component replacement](0078-connection-resources-and-component-replacement.md) | accepted | 2026-09-25 | owner+lead |
| ADR-0079 | [Verified module releases and installation](0079-verified-module-releases-and-installation.md) | accepted | 2026-09-25 | owner+lead |
| ADR-0080 | [Execution manifests in journals](0080-execution-manifests-in-journals.md) | accepted | 2026-09-25 | lead |
| ADR-0081 | [Native foundation and runtime components](0081-native-foundation-and-runtime-components.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0082 | [Component ABI and execution ownership](0082-component-abi-and-execution-ownership.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0083 | [Privileged process and completion capabilities](0083-privileged-process-and-completion-capabilities.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0084 | [Reloadable policies](0084-reloadable-policies.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0085 | [Runtime delegation and workflow modules](0085-runtime-delegation-and-workflow-modules.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0086 | [Provider components with native authenticated transport](0086-provider-components-with-native-authenticated-transport.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0087 | [Module identity and verified loading](0087-module-identity-and-verified-loading.md) | accepted | 2026-09-26 | lead |
| ADR-0088 | [Workspace capabilities across components](0088-workspace-capabilities-across-components.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0089 | [Cleartext chat endpoints on loopback hosts only](0089-cleartext-chat-endpoints-on-loopback-hosts-only.md) | accepted | 2026-09-26 | owner+lead |
| ADR-0090 | [A busy WebSocket session is waited for through the provider component](0090-a-busy-websocket-session-is-waited-for-through-the-provider-component.md) | accepted | 2026-09-27 | lead |
| ADR-0091 | [Process service and finish outcome rehome to the host runtime](0091-process-service-and-finish-outcome-rehome-to-the-host-runtime.md) | accepted | 2026-09-27 | lead |
| ADR-0092 | [Call-scoped capability services and a hostcall budget sized for whole files](0092-call-scoped-capability-services-and-a-hostcall-budget-sized-for-whole-files.md) | accepted | 2026-09-27 | lead |
| ADR-0093 | [Route settings validation is host-owned and the native route constructors leave p1-host](0093-route-settings-validation-is-host-owned-and-the-native-route-constructors-leave-p1-host.md) | accepted | 2026-09-27 | lead |
| ADR-0094 | [Resume scanner and workflow report formatter move into the foundation crates](0094-resume-scanner-and-workflow-report-formatter-move-into-the-foundation-crates.md) | accepted | 2026-09-27 | lead |
| ADR-0095 | [File-tool capability services live in the runtime; no native file-tool fallback](0095-file-tool-capability-services-live-in-the-runtime-no-native-file-tool-fallback.md) | accepted | 2026-09-27 | lead |
| ADR-0096 | [Bubblewrap credential masks follow workspace mounts](0096-bubblewrap-credential-masks-follow-workspace-mounts.md) | accepted | 2026-09-27 | lead |
| ADR-0097 | [GitHub-hosted CI provisions bubblewrap and requires the sandbox suites](0097-github-hosted-ci-provisions-bubblewrap-and-requires-the-sandbox-suites.md) | accepted | 2026-09-29 | owner+lead |
| ADR-0098 | [Write-gate waiter count is public test observability](0098-write-gate-waiter-count-is-public-test-observability.md) | accepted | 2026-09-29 | lead |
| ADR-0099 | [Mutating tools bind their planning read and cancellation to the workspace handle](0099-mutating-tools-bind-their-planning-read-and-cancellation-to-the-workspace-handle.md) | accepted | 2026-09-29 | lead |
| ADR-0100 | [Provider transport exposes bounded SSE and WebSocket seams](0100-provider-transport-exposes-bounded-sse-and-websocket-seams.md) | accepted | 2026-09-29 | lead |
| ADR-0101 | [Bounded read and listing refusals for the file tools](0101-bounded-read-and-listing-refusals-for-the-file-tools.md) | accepted | 2026-09-29 | lead |
| ADR-0102 | [Command evidence is the host's observed process exit, never component text](0102-command-evidence-is-the-host-s-observed-process-exit-never-component-text.md) | accepted | 2026-09-29 | lead |
| ADR-0103 | [Assembled tools carry their capability snapshot, not a process-global registry](0103-assembled-tools-carry-their-capability-snapshot-not-a-process-global-registry.md) | accepted | 2026-09-29 | lead |
| ADR-0104 | [Credential index rebuilds keep earlier identities and expose a test clock](0104-credential-index-rebuilds-keep-earlier-identities-and-expose-a-test-clock.md) | accepted | 2026-09-29 | lead |
| ADR-0105 | [The gate runs on GitHub-hosted runners only; the stream boxes are retired](0105-the-gate-runs-on-github-hosted-runners-only-the-stream-boxes-are-retired.md) | accepted | 2026-09-30 | owner |
| ADR-0106 | [Edit falls back to a whitespace-tolerant match and shows the closest region](0106-edit-falls-back-to-a-whitespace-tolerant-match-and-shows-the-closest-region.md) | accepted | 2026-09-30 | lead |
| ADR-0107 | [p1 builds and tests locally; GitHub Actions keeps only the required gate](0107-p1-builds-and-tests-locally-github-actions-keeps-only-the-required-gate.md) | accepted | 2026-09-30 | owner+lead |
| ADR-0108 | [Credentials are registered values masked everywhere, and credential files are written through a pinned directory](0108-credentials-are-registered-values-masked-everywhere-and-credential-files-are-written-through-a-pinned-directory.md) | accepted | 2026-09-29 | lead |
| ADR-0109 | [Tool output is stored redacted before truncation and paged through a tool-outputs capability](0109-tool-output-is-stored-redacted-before-truncation-and-paged-through-a-tool-outputs-capability.md) | accepted | 2026-09-30 | owner+lead |
| ADR-0110 | [Credential sources are bound to endpoint origins](0110-credential-sources-are-bound-to-endpoint-origins.md) | accepted | 2026-10-05 | lead |
| ADR-0111 | [Workspace mutations replace a leaf by exchange and refuse multiply linked files against an unsettled credential index](0111-workspace-mutations-replace-a-leaf-by-exchange-and-refuse-multiply-linked-files-against-an-unsettled-credential-index.md) | accepted | 2026-10-01 | lead |
| ADR-0112 | [One engine per process, deadlines on the tick clock and a digest-keyed compiled-component memo](0112-one-engine-per-process-deadlines-on-the-tick-clock-and-a-digest-keyed-compiled-component-memo.md) | accepted | 2026-10-01 | lead |
| ADR-0113 | [Releases ship ahead-of-time compiled components pinned by their own digest](0113-releases-ship-ahead-of-time-compiled-components-pinned-by-their-own-digest.md) | accepted | 2026-10-01 | owner+lead |
| ADR-0114 | [A workflow run ends when the process outgrows its memory budget](0114-a-workflow-run-ends-when-the-process-outgrows-its-memory-budget.md) | accepted | 2026-10-05 | owner+lead |
| ADR-0115 | [A bounded directory-listing interface and the ls tool](0115-a-bounded-directory-listing-interface-and-the-ls-tool.md) | accepted | 2026-10-05 | owner+lead |
| ADR-0116 | [A user-questions capability and the ask_user_question tool](0116-a-user-questions-capability-and-the-ask-user-question-tool.md) | accepted | 2026-10-05 | lead |
| ADR-0117 | [Background shell jobs on a process-jobs capability](0117-background-shell-jobs-on-a-process-jobs-capability.md) | accepted | 2026-10-05 | lead |
| ADR-0118 | [Parallel execution of concurrency-safe tool calls](0118-parallel-execution-of-concurrency-safe-tool-calls.md) | proposed | 2026-10-06 | owner+lead |
| ADR-0119 | [Per-task Cargo targets with data-tier overflow and three concurrent builds](0119-per-task-cargo-targets-with-data-tier-overflow-and-three-concurrent-builds.md) | accepted | 2026-10-06 | owner+lead |
| ADR-0120 | [An accepted finish ends the turn without another request](0120-an-accepted-finish-ends-the-turn-without-another-request.md) | proposed | 2026-10-06 | lead |
| ADR-0121 | [Journal records carry wall-clock time and request timing](0121-journal-records-carry-wall-clock-time-and-request-timing.md) | proposed | 2026-10-07 | lead |
| ADR-0122 | [A per-run scratch directory outside the workspace](0122-a-per-run-scratch-directory-outside-the-workspace.md) | proposed | 2026-10-07 | lead |
| ADR-0123 | [A timed-out shell command continues as a background job](0123-a-timed-out-shell-command-continues-as-a-background-job.md) | proposed | 2026-10-07 | lead |
| ADR-0124 | [A leaf review environment that reports to the scratch directory](0124-a-leaf-review-environment-that-reports-to-the-scratch-directory.md) | accepted | 2026-10-07 | lead |
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
