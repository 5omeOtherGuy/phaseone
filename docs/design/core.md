# Agent core — behaviour specification

Authoritative for `crates/p1-core`. Types: `crates/p1-contracts`. Fakes for tests:
`crates/p1-testkit`. Where this note and code comments disagree, this note wins;
rulings on ambiguities are appended under **Rulings** and are part of the spec.

The core runs ONE agent. It is built from `AgentParts` and owns exactly the tools it
was given. It depends on `p1-contracts` only.

## 1. Construction — `Agent::new(parts)`

Fails before anything runs:
- two tools with the same `declaration().name` → `BuildError::DuplicateToolName(name)`;
- `provider.validate(&request)` fails, where `request` is the request the first turn would
  send with an EMPTY history (`system_prompt`, all tool declarations in the order given,
  `options`) → `BuildError::ProviderRejected(error)`.

Construction commits nothing and emits nothing.

## 2. Records and sequence numbers

`seq` starts at 0 and increases by exactly 1 per committed record, across turns. A record
whose commit FAILED consumes no sequence number that a later record could reuse — after a
failed commit the agent commits nothing more in that turn (§7).

The first committed record of an agent's life is `Environment` (seq 0), committed at the
start of the first turn, BEFORE that turn's `UserInput`/`Inbox` record. It carries
`provider.describe()`, the system prompt, each tool's `(declaration, identity)` in the
order given, and the options. It is committed once, not once per turn.

## 3. One turn — `run_turn(input, cancel)`

Events are written `[Event]`, committed records `{Record}`.

1. `[TurnStarted]`. On the first turn: `{Environment}`.
2. `{UserInput{text}}`, then push `Item::User{text}` to the history.
3. **Request loop** — `request_index` counts from 0 within the turn:
   a. **Inbox.** Take every pending inbox message, in arrival order. For each:
      `{Inbox{kind,text}}`, push `Item::Inbox{kind,text}`. If at least one was taken:
      `[InboxDelivered{count}]`.
   b. **Context.** `context.prepare(&history)`. `Ok(None)` → unchanged.
      `Ok(Some(items))` → `{ContextReplaced{items}}`, and the history IS `items` from now on.
      `Err(e)` → the turn ends with `TurnEnd::ContextFailed{message: e.0}`.
   c. `[RequestStarted{request_index}]`. Call `provider.stream(request, cancel_child)` with
      `system_prompt`, the whole current history, all tool declarations (same order as
      given), `options`.
      `Err(error)` → `{AssistantInterrupted{reason: ProviderFailed, partial_text: "", error: Some(error)}}`,
      turn ends `TurnEnd::ProviderFailed{error}`.
   d. **Consume the stream**, racing every wait against `cancel`:
      - `TextDelta{text,..}` → `[TextDelta{text}]`, and append `text` to this response's partial text.
      - `ReasoningDelta{text,..}` → `[ReasoningDelta{text}]`.
      - `ToolInputDelta{call_id,text}` → `[ToolInputDelta{call_id,text}]`. Never executed, never stored.
      - `Activity` → nothing.
      - `Finished(Completed(response))` → step e. The stream is dropped; later events are never read.
      - `Finished(Failed(error))` → `{AssistantInterrupted{ProviderFailed, partial_text, Some(error)}}`,
        turn ends `ProviderFailed{error}`.
      - `Finished(Cancelled)`, OR `cancel` fires while waiting (even if the stream never
        reacts) → `{AssistantInterrupted{Cancelled, partial_text, None}}`, turn ends `Cancelled`.
      - the stream ENDS without `Finished` → treated as
        `Failed(ProviderError{kind: Transport, message: "stream ended without a terminal event"})`.
      An interrupted response adds NOTHING to the history.
   e. `{AssistantCompleted{item,stop,usage}}`, push `Item::Assistant(item)`,
      `[ResponseCompleted{model: item.origin.model, stop, usage}]`. `usage: None` stays `None`.
   f. **No tool calls in the item:**
      - `stop == Paused` → go to 3a (send the same history again).
      - otherwise, if inbox messages are pending → go to 3a.
      - otherwise the turn ends `TurnEnd::Completed{stop}`.
   g. **Tool calls**, strictly sequentially, in block order (§4). Then go to 3a.
4. Every turn end, whatever the reason: `[TurnFinished{end}]` is the LAST event, and
   `run_turn` returns the same `end`.

`run_inbox_turn(cancel)`: returns `None` immediately (no event, no record) if no inbox
message is pending. Otherwise it is `run_turn` without step 2.

## 4. One tool call

For each `ToolCall` of the completed item, in order:

| Case | Records | Events | Result pushed to history |
|---|---|---|---|
| `cancel` already fired | `{ToolFinished}` | `[ToolFinished]` | status `Cancelled`, content `Cancelled before execution.` |
| no assembled tool has `call.name` | `{ToolFinished}` | `[ToolFinished]` | status `Unavailable`, content ``Tool `<name>` is not available.`` |
| policy returns `Deny{reason}` | `{ToolFinished}` | `[ToolFinished]` | status `Denied`, content = `reason` |
| policy returns `Permit` | `{ToolStarted{call_id, identity}}` then `{ToolFinished}` | `[ToolStarted{call}]` then `[ToolFinished{result}]` | status + content exactly as the tool returned |

- Lookup is by exact `declaration().name` among THIS agent's tools. Nothing else is ever
  dispatchable — not a tool another agent owns, not a name from old history.
- Authorization sees `{call, identity, effect: tool.effect(call)}`. It is asked only for
  tools that exist, and never for a call when `cancel` has already fired.
- `{ToolStarted}` is committed BEFORE `execute` is called. If that commit fails, the tool is
  not executed.
- The core passes a cancellation token to `execute` that fires when the turn's `cancel`
  fires, and AWAITS the tool's return (it does not drop a running tool).
- The result item is `ToolResultItem{call_id, name: call.name, status, content}`.
- After the last call: if `cancel` has fired, the turn ends `TurnEnd::Cancelled` (every
  call has a result in the history by then, so the history stays well-formed);
  otherwise go to 3a.

Invalid input is the tool's business: the core never parses `ToolInput`.

## 5. Inbox

`Agent::inbox()` returns a clonable `Send` handle; `send(kind, text)` never blocks and
returns `false` once the agent has been dropped. Messages are delivered only at step 3a,
in arrival order, and each is delivered exactly once. A message sent while the agent is
idle stays pending until the next turn. `has_pending_inbox()` reports pending messages;
`inbox_ready()` resolves as soon as at least one is pending (immediately if so already)
and does not consume it.

## 6. Cancellation

`cancel` is per turn. The core never waits on the provider without also waiting on
`cancel`. After a cancelled turn the agent is reusable: a later `run_turn` with a fresh
token works and the history contains no partial response.

## 7. Commit failure

If any commit fails, the turn ends at once with `TurnEnd::CommitFailed{message}` where
`message` is the `CommitError`'s text (`error.0`). Nothing after the failed commit
happens: no further commit, no provider request, no tool execution, no history change
for that record. `[TurnFinished]` is still emitted.

## 8. Threading

`Agent` is `Send`; `run_turn` returns a `Send` future; `Inbox` is `Send + Sync + Clone`.
The core spawns no tasks and needs no particular runtime flavour.

## Not in this slice
Parallel tool execution; retries (adapters retry, the core does not); turn-completion
policies; after-tool interception; resuming from journal records (increment 5).

## Rulings
_None yet._
