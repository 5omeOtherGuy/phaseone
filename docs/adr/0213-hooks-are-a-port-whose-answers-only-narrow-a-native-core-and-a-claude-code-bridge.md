---
adr: 213
title: Hooks are a port whose answers only narrow, with a native core and a Claude Code bridge
status: accepted
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0037, ADR-0071, ADR-0153, ADR-0208, issue #711, issue #886, docs/design/seams.md §3]
---
# ADR-0213: Hooks are a port whose answers only narrow, with a native core and a Claude Code bridge

## Context

The owner, 2026-10-10, answering Q9 of the seam catalog ("hooks compatible with Claude Code or
Codex"): plan a core plus the Claude Code bridge; the Codex bridge is optional (seam #711). p1
had no hook: the brain shadow hook of ADR-0058 left with ADR-0208.

`policy.rs` names the two points where the core asks outside code: `AuthorizationPolicy` before a
tool call and `ContextPolicy` before a request. It also says "a new kind of interception is a
deliberate contract change". A policy module in WebAssembly holds no `process` capability, so a
hook command cannot run inside one.

## Decision

- **The port.** `p1_contracts::hook` names the events (`HookEvent`: the seven Claude Code and
  dsh use), a hook's answer (`HookDecision`: allow, ask or deny; `HookOutput`: a decision,
  context and a stop) and `HookRunner`, which runs the hooks of one event and merges their
  answers: deny over ask over allow, the first stop sticks, context accumulates in file order.
- **Hooks only narrow.** Their answers reach the core only through the two decision points that
  exist, as decorators composed at the root:
  - `PreToolUse` → `HookedAuthorization`, in front of the session's `AuthorizationPolicy`. A deny
    or stop refuses the call; an ask goes to the policy that asks the user, or refuses when the
    session cannot ask; an allow grants nothing, and the session's own policy still decides.
  - `SessionStart` and `UserPromptSubmit` → `HookedContext`, behind the session's
    `ContextPolicy`. Their text reaches the model labelled
    (`<hook-context event="…">`); a deny or stop fails the preparation with the hook's reason.
  - The other four events are read and not run (`HookEvent::is_served`). `PostToolUse` feedback,
    `Stop` continuation and `updatedInput` wait for the contract change `policy.rs` names.
- **The core is native** (`p1-hooks`): matching (`Matcher`: any, exact names or a whole-subject
  regular expression), the runner (`Hooks`) and the process (`ShellProcess`: `sh -c` in the
  workspace with a cleared environment holding only what the root passes, output bounded to
  1 MiB, killed on timeout or cancellation). A hook that fails, runs too long or answers
  unreadably is reported and blocks nothing.
- **A bridge is an adapter per harness** behind `HookCodec` (subject, encode, decode) plus its
  file reader. The first is `p1-hooks-claude-code`: Claude Code's three settings files in its
  order, its hook input with its tool names (`shell` is `Bash`), and its output (exit 2 blocks
  with standard error; exit 0 JSON `permissionDecision`, `decision`, `continue: false`,
  `additionalContext`; plain text is context for prompt and session hooks; any other exit is a
  non-blocking failure).

## Consequences

- A Claude Code hook that guards a tool or adds prompt context runs unchanged under p1.
- A hook can never grant what the session would refuse, and an installed hook cannot reach the
  credentials the host was started with through its environment.
- Nothing switches the bridge on yet: the environment or settings key and the
  `hook/invoked` / `hook/result` journal records are the next slice. Until then the crates are
  composed only by their tests.
- `HookedContext` runs `UserPromptSubmit` once per new trailing user item; a resumed session
  whose history ends in a user item runs it once more.

## Alternatives considered

- Hooks as a policy module in WebAssembly: rejected, a policy module holds no `process`
  capability, and giving one would widen the class for every policy.
- Honouring a hook's `allow` as a grant, as Claude Code does: rejected; a hook narrows, the
  session's authorization and its confinement stay the floor.
- Running hooks through the tools' `ProcessService`: rejected for now; it is a tool service
  bound to a tool's confinement and grants, and a hook is the user's own command. `HookProcess`
  keeps the boundary swappable.

## Evidence

- `cargo test -p p1-contracts hook` (merge order, first stop, served events).
- `cargo test -p p1-hooks`: matching, the runner's order, merge and failure report, a real
  `sh` hook with a cleared environment, timeout and cancellation, and both decorators
  (`crates/p1-hooks/tests/core.rs`).
- `cargo test -p p1-hooks-claude-code`: settings files and notes, the input, every decode case,
  and a Claude Code hook that exits 2 refusing a p1 `shell` call end to end
  (`crates/p1-hooks-claude-code/tests/bridge.rs`).
