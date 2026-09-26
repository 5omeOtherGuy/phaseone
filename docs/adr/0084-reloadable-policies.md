---
adr: 84
title: Reloadable policies
status: proposed
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [issue #238, epic #206, DECISIONS.md D22, migration plan finding F8, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0024-authorization-permit-deny-at-the-core.md, docs/adr/0036-context-control-is-a-summarizing-policy-module-with-a-durable-validated-replacement.md, docs/adr/0038-full-access-is-the-default-asking-is-opt-in.md, docs/adr/0049-model-selection-and-switching-a-session-to-another-model.md, docs/adr/0078-connection-resources-and-component-replacement.md, docs/adr/0021-journal-is-the-single-truth.md, docs/adr/0068-tool-output-is-masked-for-credential-shapes-before-history-journal-and-summaries.md, docs/adr/0080-execution-manifests-in-journals.md, docs/design/modules/wit.md, docs/design/modules/package.md, crates/p1-host/src/policy.rs, crates/p1-tui/src/runtime.rs]
---
# ADR-0084: Reloadable policies

## Context

ADR-0071 (DECISIONS.md D22) makes every context policy and authorization policy a WebAssembly
module the host loads by name. Three accepted decisions were written for policies that were
native objects built once per process:

- ADR-0024: the core's authorization outcome is exactly `Permit` or `Deny`; "ask" is resolved
  inside the host's policy and reaches the tool only as a decision.
- ADR-0036: context control is a stateless summarizing policy; the core validates call/result
  pairing before it journals `ContextReplaced` and announces the replacement after the commit.
- ADR-0038: full access is the default policy object, `--ask` opts into the restrictive one, and
  workers share the parent's policy.

The frozen boundary (`wasm-boundary-v1`, `docs/design/modules/wit.md`) already fixes the shape of
the two policy worlds. `context-policy` exports `configure`, `prepare` and `compact-now` and
summarizes through the native `summary` capability. `authorization-policy` exports `authorize`,
which by decision S0-R2.1 returns the world-local `verdict` (`permit`, `deny`, `ask`); the host
resolves `ask`, and `types.decision` is unchanged. The allocation gives an authorization policy
`control`, `clock` and `notices` only. A package's identity is the digest of its built bytes
(`docs/design/modules/package.md`); the manifest adds `name`, `variant` and `capabilities`.

ADR-0078 makes a model switch (ADR-0049) and a module reload one between-turns replacement of a
component. It leaves the reload journal records and the approval keys to this decision.

Today an approval is keyed by less than the code it approved. `HostPolicy` remembers an "always"
answer for the process under the tool name and its `ToolIdentity`; `TuiPolicy` keeps session
grants under the same key. Once the host loads packages by name, the same name and identity can
belong to different bytes after a release or a reload, and the same tool can be decided by a
different policy. Finding F8 of the migration review: an approval must not outlive the artifact
it was given to.

## Decision

**1. Policies become components.** Under ADR-0071:

- The shipped default is the package `p1/policy/full-access`, which permits every call; it stays
  the default under ADR-0038. `--ask` selects the restrictive package `p1/policy/ask`, which
  permits read-only calls and answers `ask` for every other effect.
- The context policy becomes the component `p1/context/summarizing` over the native `summary`
  capability. ADR-0036's durable validated replacement is kept: the core still validates
  call/result pairing and journals `ContextReplaced` before it announces the replacement.
- Summary output is masked natively (ADR-0068) before it is used in any history item or journal
  record.
- The summary operation streams on the host through the agent's provider. It holds no policy
  state, never re-enters the context component's Store and never calls `prepare` recursively.

**2. ADR-0024 is kept at the core.** Only `Permit` or `Deny` reaches `p1-core`:

- A component may answer `ask`. The native ask bridge resolves it through the current front end,
  bound to the active turn's cancellation scope. A turn cancelled while the question is open
  resolves to `Deny` with the existing cancel reason; a headless host resolves `ask` to `Deny`
  with the existing headless reason, without waiting for input.
- A policy component gets no filesystem, transport, credential, process or worker capability; it
  is linked with the `control`, `clock` and `notices` capabilities of the wit.md allocation at
  most.
- A policy trap, a stopped execution or an invalid output yields a conservative decision: `Deny`
  with a reason naming the policy package, never `Permit`.

**3. Reload.** A policy is replaced through ADR-0078's between-turns replacement; `/modules
reload` and a model switch are the same operation:

- A replacement happens only between complete turns, after outstanding tool calls settle. A
  request made while the session is busy is queued and reported as pending.
- The complete candidate assembly is validated against the current history, then committed as one
  environment record that names every module identity, so the journal shows which policy decided
  every later call. It is installed with no await between the commit and the installation.
- A failure before the commit leaves the current assembly intact and is reported.
- Running children and workflows keep the assembly generation they were started with; new ones
  take the new generation. The old generation is dropped when its last user ends.

**4. Approval keys (F8).** A persistent approval decision is keyed by all of:

- the full module identity (package name and digest) of the tool that would run;
- the full module identity (package name and digest) of the policy that decided;
- the tool's variant;
- the policy's effective configuration;
- the capability digest of the grant.

The rule covers today's "always" answers and session grants and any later persisted form. An
approval resets on every release and on every reload that changes any part of its key; a changed
key carries no old approval, so an approval never transfers to a replacement artifact. The
reason: an approval is consent to one artifact's behaviour, and a replacement is different code,
even under the same name.

**5. What changes in the amended ADRs.** ADR-0024, ADR-0036 and ADR-0038 stay accepted; this
decision amends them for the module architecture and supersedes nothing.

- ADR-0024. Unchanged: the core knows only `Permit` and `Deny`, and the policy is asked only for
  tools that exist and only when cancellation has not fired. Changed: "ask" is now a verdict a
  policy component may return, and the native ask bridge, not the policy, resolves it through the
  front end under the active turn's cancellation. A failing policy denies.
- ADR-0036. Unchanged: the policy is stateless, summarizes through the agent's own provider, and
  the core validates and journals `ContextReplaced` before announcing it. Changed: the policy runs
  as the component `p1/context/summarizing`; the summary request itself is a native capability
  that masks its output and never re-enters the component.
- ADR-0038. Unchanged: full access is the default, `--ask` opts in, workers share the parent's
  policy. Changed: the default and the opt-in are the packages `p1/policy/full-access` and
  `p1/policy/ask`, a policy can be replaced between turns, and an "always" answer is bound to the
  F8 key instead of the tool name.

**6. Not decided here.** The WIT signatures (S0), the journal record format (S1), the `p1-core`
reconfiguration operation (S1), and the loader and package identity (S0, S1).

## Consequences

- An operator re-approves after a release or a reload: every approval under a changed key is
  gone, including one for a tool whose name and behaviour look unchanged.
- The journal gains one environment record per reload, in the version-2 form with the assembly
  identity line of ADR-0080; this decision cites that format and does not define it. A replayed
  session can name the policy that decided each call.
- `--ask` and the default keep their observable behaviour: the same verdicts, the same prompt, the
  same deny reasons. The difference is where each half runs.
- A policy component cannot read input, touch the workspace or reach the network, so it can affect a
  call only through the verdict it returns. That verdict includes `permit`, and §2's conservative
  rule covers a trap, a stopped execution or an invalid output, not a policy that runs and answers
  `permit`: a faulty or replaced `p1/policy/ask` can wrongly permit every call and silently defeat
  the operator's `--ask` opt-in. The sandbox limits what a policy can touch, not what it may allow,
  so the host does not contain a wrong verdict.
- Risk: a handle held across compaction, reload and reconnect can outlive its meaning. The
  stream's reload suite (S5.7) and compaction-workload suite (S5.9) cover it; until they pass,
  the rules in §1 and §3 are stated, not proven.
- Children and workflows started before a reload keep the old policy until they end, so for a
  time two generations may decide calls in one session; the journal names both.

## Alternatives considered

- **Key approvals by tool name and `ToolIdentity`, as today.** Rejected: after a release or a
  reload the same key would approve different bytes, or the same tool under a different policy.
- **Keep approvals across a release when the tool's digest is unchanged.** Rejected: the deciding
  policy, its configuration or the capability grant may have changed, and each is part of what
  the operator consented to.
- **Resolve `ask` inside the policy component.** Rejected: the world has no way to read input,
  the question must race the active turn's cancellation, and ADR-0024 keeps the question in the
  host.
- **Replace a policy in the middle of a turn.** Rejected for the reasons ADR-0078 gives: the
  history `validate` checks would still be changing, and outstanding calls would be decided by
  two policies.

## Evidence

No measurement exists yet. This ADR is merged `proposed` before the reload slice (S5.7) lands and
is accepted, with Evidence, in the PR that lands the phase's last DoD row.
