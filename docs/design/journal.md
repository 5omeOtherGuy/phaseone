# Session journal stores and resume — specification

Contract: `crates/p1-contracts/src/journal.rs` (record kinds, commit boundaries).
The core owns ordering; a store only makes records durable and reads them back.

## Crate `p1-journal`

```rust
pub struct MemoryJournal;                 // CommitSink; records() -> Vec<JournalRecord>; Clone (shared)
pub struct JsonlJournal;                  // CommitSink
impl JsonlJournal {
    /// Create a NEW session file. Fails if the file exists.
    pub fn create(path: &Path, sync: SyncPolicy) -> Result<Self, JournalError>;
    /// RESUME: lock the file FIRST, then read, validate, cut off a truncated tail and derive
    /// the next sequence — all under that lock, held for the writer's life (ADR-0031).
    pub fn resume(path: &Path, sync: SyncPolicy) -> Result<(Self, Resumed), JournalError>;
    /// Lower-level: locks before reading; rejects a `next_seq` that does not match the file.
    pub fn open_for_append(path: &Path, sync: SyncPolicy, next_seq: u64) -> Result<Self, JournalError>;
    /// Version 2 and 3 only: an assembly identity line, durable like a commit under `SyncPolicy`.
    pub fn record_assembly(&self, identity: &AssemblyIdentity) -> Result<(), JournalError>;
    /// Stamp every version-3 record committed from now on with `at_ms` from `clock`.
    pub fn set_clock(&self, clock: Arc<dyn Clock>);
}
pub enum SyncPolicy { EveryRecord, OsBuffered }
pub fn load(path: &Path) -> Result<Loaded, JournalError>;
pub struct Loaded { pub records: Vec<JournalRecord>, pub truncated_tail: Option<TruncatedTail>,
                    pub version: u64, pub assemblies: Vec<AssemblyEntry>,
                    pub at_ms: Vec<Option<u64>> }
pub struct TruncatedTail { pub byte_offset: u64, pub bytes: u64 }
pub struct Resumed { pub records: Vec<JournalRecord>, pub repaired_tail: Option<TruncatedTail>,
                     pub version: u64, pub assemblies: Vec<AssemblyEntry>,
                     pub at_ms: Vec<Option<u64>> }
pub struct AssemblyEntry { pub from_seq: u64, pub identity: AssemblyIdentity }
/// Cut the file back to `byte_offset`. Takes the lock (`Locked` if a writer owns the file) and
/// refuses a tail that is no longer the file's tail (`StaleTail`).
pub fn repair_truncated_tail(path: &Path, tail: &TruncatedTail) -> Result<(), JournalError>;
```

**Format.** One record per line: `serde_json` of `JournalRecord`, `\n`-terminated. The first
line of a file is a header naming the format version; a version other than 1, 2 or 3 is
`JournalError::UnknownVersion`, never guessed.
- Version 1 (`{"p1_journal":1}`) holds records only. It is what the released binaries write
  and the only version they accept. p1 still reads it and appends to it as version 1: the
  header is never rewritten, and `record_assembly` on it is `AssemblyNeedsVersion2`.
- Version 2 (`{"p1_journal":2}`) permits an assembly identity line between records (see below).
  Old binaries refuse it, which is the point: they cannot check what executed the session.
- Version 3 (`{"p1_journal":3}`) is what `create` writes (ADR-0121). Besides the version-2
  assembly line it stamps every record line with `at_ms`, the store clock's wall-clock
  milliseconds since the Unix epoch, beside the record's own fields (see "Time" below). Old
  binaries refuse it. Reading, appending to and resuming a version-1 or version-2 file keeps
  its format: no `at_ms` is written, and `load`/`resume` report `at_ms` as `None` for its lines.
- The assembly identity line `{"assembly":{"environment":…,"host":{"version":…,"commit":…},"modules":[…]}}`,
  each module `{"name","kind","package","version","digest","abi"}` with `kind` one of `tool`,
  `provider`, `context_policy`, `authorization_policy`, `digest` the sha256 hex of the
  verified package bytes (`null` for a native module) and `abi` optional. The line carries no
  `seq`, does not count in the dense-seq rule, and applies to the records that follow it;
  `load` reports it as `AssemblyEntry{from_seq}` with the seq of the next record (the record
  count when it is last). Unknown fields are refused (`deny_unknown_fields`): extending the
  identity is a version bump. An assembly line in a version-1 file is
  `JournalError::AssemblyInVersion1{line}`. A torn assembly line is a truncated tail, exactly
  like a torn record. `MemoryJournal::record_assembly`/`assemblies()` mirror the file store.
Both stores reject a record whose `seq` is not exactly the next one (`JournalError::OutOfOrder`)
— gaps and repeats are bugs in the caller, caught at the store.

## Time — wall-clock per record and per request

ADR-0121. Time in a journal is wall-clock (milliseconds since the Unix epoch), not monotonic:
it survives across processes and is the same clock `at_ms` uses.

**Per record.** Every version-3 record line carries `at_ms` beside the record's own fields,
taken from the store's `Clock`. A store starts with `SystemClock`; a test (or a host that wants
a pinned clock) calls `set_clock`. `load` and `resume` expose the per-line values in `at_ms`,
parallel to `records`, `None` for a version-1/2 line. Appending to a version-1 or version-2 file
writes no `at_ms` (its format is unchanged), so a mixed file never appears.

**Per request.** Right after each request's `AssistantCompleted` (or `AssistantInterrupted`) the
core commits one `RequestTiming` record (never one per wait):

```rust
RecordBody::RequestTiming { request_index, sent_ms, first_event_ms, first_output_ms, ended_ms, waits }
// waits: Vec<Wait>, each Wait { reason: WaitReason, attempt: u32, delay_ms: u64 }
// WaitReason: rate_limited | server_error | transport | slow_first_byte | busy
```

- `sent_ms` — just before the provider stream is opened; `request_index` counts requests within
  the turn from 0.
- `first_event_ms` — at the first stream event of ANY kind (a first-byte proxy); `None` if none.
- `first_output_ms` — at the first text, reasoning or tool-input delta (a first-token proxy);
  `None` if the request produced no output.
- `ended_ms` — at the terminal event.
- `waits` — each `StreamEvent::Wait` the provider emitted, in order. A `Wait` is timing only:
  it is not history, not model input and not a substitute for `Notice`.

`RequestTiming` is never history: resume and every projection skip it outright, exactly like
`ToolStarted` and `Environment`, so it changes no model-visible state. A sink whose format does
not carry timing answers `accepts_request_timing() == false` (a version-1 or version-2 file);
the core then commits no `RequestTiming` there and `seq` stays dense. `scripts/journal-timing.py`
reads a journal and prints, per request, the time to first event, the time to first output, the
decode time and the waits.

**What `SyncPolicy` guarantees — and what it does not.**
- `EveryRecord`: `commit` returns after `write` + `fsync` of the file (and, on `create`, of the
  parent directory). A record whose commit returned survives power loss, subject to the
  drive honouring flush. This is the default for `--session`.
- `OsBuffered`: `commit` returns after `write`. Survives a process crash, not a power loss.
- Neither gives exactly-once execution of side effects: a crash between a tool's side effect
  and its `ToolFinished` record leaves that call's outcome UNKNOWN (see Resume).

**Truncated tail.** A crash can leave a final line without `\n`, or a final line that is not
valid JSON. `load` returns every complete valid record before it plus `truncated_tail`;
it never drops a complete line and never "repairs" silently. An invalid line that is NOT the
last line is `JournalError::Corrupt{line}` — that file is not resumed.

Session JSONL readers refuse files above 256 MiB before allocation, including sparse files and files grown during a read. Tail repair requires the same offset, length and observed-byte fingerprint under the writer lock; a same-length replacement is stale.

## Projection and resume — in `p1-core`

```rust
pub fn project(records: &[JournalRecord]) -> Result<Projection, ResumeError>;
pub struct Projection { pub history: Vec<Item>, pub next_seq: u64, pub environment_committed: bool,
                        pub unresolved_calls: Vec<UnresolvedCall> }
pub struct UnresolvedCall { pub call: ToolCall, pub started: Option<ToolIdentity> }
impl Agent { pub fn resume(parts: AgentParts, records: &[JournalRecord]) -> Result<(Agent, ResumeReport), ResumeError>; }
```
Projection rules: `UserInput`/`Inbox`/`AssistantCompleted`/`ToolFinished` append their item;
`ContextReplaced` replaces the history and clears the last usage; `AssistantInterrupted`,
`ToolStarted` and `Environment` append nothing. Memory and JSONL stores yield the identical
projection for the same records (acceptance: "memory and file storage preserve committed
model-visible state consistently") — tested by running the same scripted session against both.

**Interrupted-call reconciliation.** After projection, every tool call of the LAST assistant
item without a `ToolFinished` is unresolved:
- it had a `ToolStarted` → the side effect may have happened. `resume` appends (and commits)
  `ToolFinished{status: Unknown, content: "Interrupted: this call was started before the session
  stopped and its outcome is unknown. Check the current state before retrying."}`. It is NEVER
  re-executed automatically.
- it had no `ToolStarted` → it never ran: `ToolFinished{status: Cancelled, content: "Cancelled before execution."}`.
So the resumed history is well-formed (every call has a result) and the MODEL decides what to
do, with the truth in front of it.

**Environment on resume.** The journalled tool identities are compared with the newly
assembled ones. A call name whose `ToolIdentity` changed, or that no longer exists, is fine
for completed history (results are just history) and is reported in `ResumeReport`; nothing
old is ever dispatched. If the route origin (route or model) differs from the journalled one,
the resume is REJECTED with `ResumeError::RouteChanged` before anything is committed
(ADR-0033): no translation exists, so nothing establishes that the transcript is a valid
continuation elsewhere. A new `Environment` record is committed when the resolved environment
differs in any other way (prompt, tools, options).

**Worker provenance.** Direct workers and workflow step workers use the same
assembly-naming sink as the parent (ADR-0080). Their version-2 `FILE.w<N>.jsonl`
journals name the loader-verified packages of their pinned generation before the
first execution record, and name a regrant's new assembly before its `Environment`
record and repaired turn. Refused regrants keep the old assembly active; if a
candidate line reached the file before refusal, rejection immediately writes the
old identity again, even when no record follows. A refused restoration is reported
and remains owed before the next record. Cleanup only restores an identity still
owned by that candidate, never a concurrent retry's identity. The journal format
and dense sequence rule are unchanged.

**Workers on resume.** Child sessions are not restored (ADR-0034). The host tells the user
and — through the inbox — the model which workers of the earlier process are gone, and the
new worker service never reuses their ids.

**Ownership.** Every writer holds an exclusive advisory lock on the session file itself. A
second process fails with `Locked` WITHOUT having modified the file — this includes resume
and tail repair, which act only under the lock, never on an earlier unlocked observation.

## Not promised in this slice
Several writers sharing one session file (the second one fails fast, see Ownership),
compaction of journals, encryption, recovery from corruption in the middle of a file.
