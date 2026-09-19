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
    /// Open an existing session file for appending after `load` has validated it.
    pub fn open_for_append(path: &Path, sync: SyncPolicy, next_seq: u64) -> Result<Self, JournalError>;
}
pub enum SyncPolicy { EveryRecord, OsBuffered }
pub fn load(path: &Path) -> Result<Loaded, JournalError>;
pub struct Loaded { pub records: Vec<JournalRecord>, pub truncated_tail: Option<TruncatedTail> }
pub struct TruncatedTail { pub byte_offset: u64, pub bytes: u64 }
/// Cut the file back to `byte_offset` (after the caller decided to continue from it).
pub fn repair_truncated_tail(path: &Path, tail: &TruncatedTail) -> Result<(), JournalError>;
```

**Format.** One record per line: `serde_json` of `JournalRecord`, `\n`-terminated. The first
line of a file is a header `{"p1_journal":1}`; an unknown version is an error, never guessed.
Both stores reject a record whose `seq` is not exactly the next one (`JournalError::OutOfOrder`)
— gaps and repeats are bugs in the caller, caught at the store.

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

## Projection and resume — in `p1-core`

```rust
pub fn project(records: &[JournalRecord]) -> Result<Projection, ResumeError>;
pub struct Projection { pub history: Vec<Item>, pub next_seq: u64, pub environment_committed: bool,
                        pub unresolved_calls: Vec<UnresolvedCall> }
pub struct UnresolvedCall { pub call: ToolCall, pub started: Option<ToolIdentity> }
impl Agent { pub fn resume(parts: AgentParts, records: &[JournalRecord]) -> Result<(Agent, ResumeReport), ResumeError>; }
```
Projection rules: `UserInput`/`Inbox`/`AssistantCompleted`/`ToolFinished` append their item;
`ContextReplaced` replaces the history; `AssistantInterrupted`, `ToolStarted` and
`Environment` append nothing. Memory and JSONL stores yield the identical projection for the
same records (acceptance: "memory and file storage preserve committed model-visible state
consistently") — tested by running the same scripted session against both.

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
old is ever dispatched. If the route origin differs from the journalled one, replay data of
the old origin is dropped by the adapter (providers.md) — `ResumeReport` says so. A new
`Environment` record is committed when the resolved environment differs.

## Not promised in this slice
Multi-writer safety on one session file (a lock file makes a second writer fail fast),
compaction of journals, encryption, recovery from corruption in the middle of a file.
