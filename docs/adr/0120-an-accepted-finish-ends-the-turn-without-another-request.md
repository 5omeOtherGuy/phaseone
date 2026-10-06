---
adr: 120
title: An accepted finish ends the turn without another request
status: proposed
date: 2026-10-06
deciders: lead
supersedes: []
superseded_by: []
sources: [crates/p1-core/src/lib.rs, crates/p1-contracts/src/tool.rs, crates/p1-tool-finish/src/lib.rs, ~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md]
---
# ADR-0120: An accepted finish ends the turn without another request

## Context

The core loop runs a response's tool calls and then always sends another request; a turn ends
only on a response without tool calls (`crates/p1-core/src/lib.rs`, step 3g). After an accepted
`finish` the model has nothing left to do, yet p1 pays one more full-context request for an
empty answer. The 2026-09-27 harness analysis saw this in 20 of 20 runs that ended with an
accepted `finish`; W0's last request was 149k uncached tokens (issue #425). Codex and Claude
Code end a task on their completion signal without a further request.

The core must not know the `finish` tool by name (AGENTS.md "Architecture": no tool names in
`p1-core`).

## Decision

1. `p1_contracts::Tool` gains one provided method, `ends_turn(&self, outcome: &ToolOutcome) ->
   bool`, default `false`. A tool returns `true` only for an outcome after which the agent's
   turn is over. Wrappers that delegate to an inner tool (the host's faced and bound tools)
   forward it.
2. `FinishTool` returns `true` exactly for an accepted call (status `Ok` with an accepted
   outcome recorded); a rejected `finish` keeps the turn going so the model can repair it.
3. After running all tool calls of a response, the core ends the turn with
   `TurnEnd::Completed { stop }` (the response's own stop reason) when any call's outcome ended
   the turn and the turn was not cancelled. Pending inbox messages do not keep it alive: an
   accepted `finish` is the agent's declaration that its task is done, and the host decides
   what happens next, as it does today after the extra request.
4. Every tool call of that response still runs and records its result before the turn ends,
   so the history never holds a call without a result.

## Consequences

- Every run that ends with an accepted `finish` makes one request fewer; its journal ends with
  the `finish` result.
- A model that calls `finish` together with other tools in one response still gets those calls
  executed and recorded; it gets no chance to react to their results after the accepted
  `finish`.
- Frontends see `Completed` with stop reason `tool_use` for such turns and must render it as a
  normal completion.

## Alternatives considered

- **A field on `ToolOutcome`:** about 110 struct literals across crates and tests would change
  for one bit that only `finish` sets.
- **The core matching the tool name `finish`:** breaks the rule that the core knows no tool
  names.
- **Keep the extra request:** costs a full-context request on every finished run.

## Evidence

- `~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md` (2026-09-27): 20 of 20 accepted-finish
  runs made one more request; W0's was 149k uncached.
- Behaviour tests in `crates/p1-core/tests` and `crates/p1-tool-finish/tests` (this change).
