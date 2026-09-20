# Optional delegation — specification

Delegation is a TOOL MODULE plus a worker service. Nothing in the core knows about it;
without these crates in the composition the harness is a plain coding agent (acceptance:
the host builds and works with the `delegation` cargo feature off).

## Crates

| Crate | Owns |
|---|---|
| `p1-workers` | The typed worker API (`WorkerService` trait, ids, statuses) AND the in-process implementation. Depends on `p1-contracts` + `p1-core`. |
| `p1-tool-delegate` | The model-facing tools. Depends on the `WorkerService` trait only, not on the implementation's internals. |

## Worker API

```rust
pub struct ChildId(pub String);                       // "w1", "w2", … per service
pub enum ChildStatus { Running, Finished(ChildResult), Cancelled, Failed(String) }
pub struct ChildResult { pub final_text: String, pub turn_end: TurnEnd, pub usage_total: Option<Usage> }
pub struct ChildSpec { pub environment: String, pub task: String, pub workspace: Option<PathBuf> }

pub trait WorkerService: Send + Sync {
    /// Starts NOW (not when someone polls). Err if the environment is unknown/invalid,
    /// or the concurrency bound is reached.
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>>;
    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;
    /// Resolves when the child is no longer Running. Cancel-safe; may be called repeatedly.
    fn wait<'a>(&'a self, id: &'a ChildId, cancel: CancellationToken) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>>;
    /// Another turn in the SAME child session (repair). Err(Busy) while a turn is running.
    fn continue_child<'a>(&'a self, id: &'a ChildId, message: String) -> BoxFuture<'a, Result<(), WorkerError>>;
    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>>;
}
```

In-process implementation `InProcessWorkers::new(factory, parent_inbox, max_concurrent)`:
- `factory: Arc<dyn Fn(&ChildSpec) -> Result<Agent, String> + Send + Sync>` is injected by the
  host and is the SAME assembly path a top-level agent uses — so a child on the other route
  gets that route's prompt and tools, and nothing of the parent's. The child receives the task
  text only, never the parent's transcript.
- Each child runs as one tokio task that owns its `Agent` (single owner; turns are serialised,
  so `continue_child` can never race a running turn).
- **Completion is retained state plus a notification.** When a child's turn ends the service
  (1) stores the status — retrievable by id for the service's lifetime, however late anyone
  asks — and only then (2) sends ONE inbox message to the parent:
  `Worker <id> finished (<completed|cancelled|failed>). Use worker_result to read its result.`
  with `InboxKind::Notification`. A missed or ignored notification loses nothing.
- The notification wakes the parent at its next safe boundary (core §3a/§3f, R4); an idle
  parent is woken by the host via `Agent::inbox_ready()` → `run_inbox_turn`.
- `max_concurrent` bounds RUNNING children (default 2); `start` AND `continue_child` beyond it
  are an error the model can read, not a queue. Every transition into Running is one critical
  section: limit check, status change and the new turn's cancellation token together — so a
  `cancel` or `shutdown` right after an accepted `start`/`continue_child` always reaches that
  turn, even before the child task has been polled. No recursion in this slice: child environments are assembled
  WITHOUT the delegation tools.
- Cancelling the parent's service (`shutdown()`) cancels every running child and joins the
  tasks; dropping it does the same best-effort. Child authorization: the parent's policy
  object is shared, so `--yes` or an "always" grant applies and a headless deny stays a deny.
- Children share the parent's workspace by default. Their FILE-TOOL mutations are serialized
  with the parent's and each other's (one `WriteGate` per catalog, ADR-0032): check-and-write
  is one step across agents, so nobody silently overwrites a change they have not seen — the
  later writer gets the ordinary stale-file error. Shell commands are not covered: the tool
  description and the prompt still tell the model not to give overlapping files to workers.
  Isolation (a worktree per child) is later.

- Workers do not survive the process. When the parent session is resumed, the host declares
  the journalled workers gone (stderr + an inbox notification to the model) and reserves
  their ids, so an old id answers `No worker <id>.` and is never given to a new worker
  (ADR-0034).

## Model-facing tools (`p1-tool-delegate`, effect `Delegates`)

| Tool | Input | Output content |
|---|---|---|
| `worker_start` | `{"environment": string, "task": string}` | `Started worker <id> on <route>/<model>. You will be notified when it finishes.` |
| `worker_result` | `{"id": string, "wait"?: bool}` | status line + the child's final text; `wait:true` blocks until it is no longer running (cancellable) |
| `worker_continue` | `{"id": string, "message": string}` | `Worker <id> continues.` — for repair in the same child session |
| `worker_cancel` | `{"id": string}` | `Worker <id> cancelled.` |

Unknown id → error `No worker <id>.` Execution completed is not work accepted: the
description tells the model to verify a child's result before relying on it.

## Behaviour tests (from the owner's observed failures)
- F2: a child finishing while the parent is mid-turn, idle, or blocked in `worker_result{wait}`
  always reaches the parent; with the notification dropped on purpose, `worker_result` still
  returns the retained result.
- "An unassembled tool cannot be dispatched": with delegation absent, a model that invents a
  `worker_start` call gets `Unavailable` (core §4).
- A child on the OTHER route sees only its own environment's tool declarations and prompt
  (assert on the scripted child provider's recorded request).
- Repair keeps state: `worker_continue` sends the message into the same history.
