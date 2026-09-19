# Tool modules — specification

Every tool is its own crate implementing `p1_contracts::Tool`. A tool gets what it needs
from its constructor (workspace, limits, shared file-observation state); there is no
tool-state bag and no global. Tools contain no provider wire formats and no UI types.
Bodies are extracted from iris-agent (`src/tools/`), adapted to these contracts.

## Crates

| Crate | Tool(s) | Effect | Donor |
|---|---|---|---|
| `p1-workspace` (helper library, not a tool) | path confinement, atomic write, observed-file registry, output bounding | — | `tools/path.rs`, `tools/text.rs`, `ObservedFiles` in `tools/mod.rs`, atomic write in `tools/write.rs`/`edit.rs` |
| `p1-tool-read` | `read` | `ReadOnly` | `tools/read.rs` (without `skim`, without skill roots) |
| `p1-tool-edit` | `edit` | `WritesFiles` | `tools/edit.rs` |
| `p1-tool-write` | `write` | `WritesFiles` | `tools/write.rs` |
| `p1-tool-search` | `grep` | `ReadOnly` | `tools/grep.rs` (+ the `find` glob listing as mode `files`) |
| `p1-tool-shell` | `shell` | `Executes` | `tools/bash/mod.rs` one-shot path only (no sessions, jobs, sandbox) |
| `p1-tool-patch` | `apply_patch` | `WritesFiles` | new (V4A patch format); shares `p1-workspace` |

Tools may depend on `p1-contracts`, `p1-workspace` and ordinary libraries — never on each
other, on `p1-core`, or on a provider.

## `p1-workspace`

```rust
pub struct Workspace { /* canonical root */ }
impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError>;   // canonicalizes; must be a directory
    pub fn root(&self) -> &Path;
    /// Resolve a model-supplied path (relative to root, or absolute) to a canonical
    /// path INSIDE the root. For a path that does not exist yet, the deepest existing
    /// ancestor is canonicalized and must be inside the root.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, WorkspaceError>;
    pub fn display(&self, path: &Path) -> String;                         // root-relative, `/` separators
}
pub enum WorkspaceError { NotADirectory(..), OutsideWorkspace{requested}, Io{..} }

/// Atomic replace: write a sibling temp file, fsync, rename over the target; keeps the
/// target's permission bits; creates missing parent directories.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()>;

/// Which files this AGENT has seen, and in what state. Shared (`Clone`, internally
/// `Arc<Mutex<_>>`) between the read/edit/write/patch tools of ONE agent.
pub struct ObservedFiles;
impl ObservedFiles {
    pub fn new() -> Self;
    pub fn record(&self, path: &Path, contents: &[u8]);      // after a successful read or write
    pub fn check_unchanged(&self, path: &Path, current: &[u8]) -> Observation;
}
pub enum Observation { NeverObserved, Unchanged, ChangedSinceObserved }

/// Bound text shown to the model: at most `max_bytes` (cut on a char boundary) and
/// `max_lines`; when cut, append `\n[output truncated: showing <shown> of <total> bytes]`.
pub fn bound_output(text: &str, max_bytes: usize, max_lines: usize) -> String;
```

**Confinement invariant (always on — not a permission setting):** every path a file tool
touches resolves inside the workspace root AFTER symlink resolution. `..` escapes, absolute
paths outside the root and symlinks pointing outside are all `OutsideWorkspace`. Must-pass
examples, root `/w`: `src/a.rs` ok · `/w/src/a.rs` ok · `../x` rejected · `/etc/passwd`
rejected · `link/secret` where `link -> /etc` rejected · `new/dir/file.txt` (nothing exists
yet) ok · `link2/new.txt` where `link2 -> /tmp` rejected.

**Read-before-mutate invariant:** `edit` and `write` (when the target exists) refuse a file that this agent has `NeverObserved`
(`You must read <path> before changing it.`) or that `ChangedSinceObserved`
(`<path> changed on disk since you last read it; read it again.`). A successful mutation
records the new contents, so consecutive edits need no re-read. `apply_patch` is exempt: its
hunks must match the file's CURRENT contents, which is its own staleness check, and the GPT
environment reads files through `shell`, which this registry cannot see. It still records
what it wrote.

## Common rules for every tool

- Input is `ToolInput::Json(raw)` for function tools: parse `raw` with serde into the tool's
  input struct with `deny_unknown_fields`. ANY parse or validation failure →
  `ToolOutcome::error("Invalid input for <tool>: <reason>")`. Never panic, never repair.
  A `ToolInput::Text` given to a function tool (or vice versa) is the same error.
- Blocking filesystem/process work runs in `tokio::task::spawn_blocking` or async I/O, not on
  the async thread. Cancellation returns `ToolStatus::Cancelled` promptly.
- Errors the model can act on are `ToolStatus::Error` with a one-line message naming the path.
- Model-visible content is bounded with `bound_output` (defaults: 50_000 bytes, 2_000 lines).
- `declaration()` text below is the Claude-family variant (`variant: "claude"`). Constructors
  take a `ToolFace { name, description }` override so another family can present the same
  implementation differently (seams §4 variant rule); schema stays with the implementation.

## `read`  — `{"path": string, "offset"?: int>=1 (default 1), "limit"?: int>=1 (default 2000)}`
Returns lines `offset..offset+limit` formatted `<line number right-aligned to 6>\t<text>`
(donor format). Records the FULL file contents as observed. Errors: missing file, directory,
binary file (contains NUL in the first 8 KiB: `<path> is a binary file.`), outside workspace.
When more lines remain: final line `[<n> more lines; continue with offset=<next>]`.
Empty file: `<path> is empty.`

## `edit` — `{"file_path": string, "old_string": string (non-empty), "new_string": string, "replace_all"?: bool}`
Exact string replacement. `old_string == new_string` → error. 0 matches → error
`old_string was not found in <path>.` >1 matches without `replace_all` → error
`old_string occurs <n> times in <path>; add context to make it unique or set replace_all.`
Preserves the file's line endings and trailing newline. Atomic write. Success content:
`Edited <path> (<n> replacement(s)).`

## `write` — `{"file_path": string, "content": string}`
Creates or replaces a file atomically (parents created). Existing target → read-before-mutate
applies. Success: `Wrote <path> (<bytes> bytes).`

## `grep` — `{"pattern": string, "path"?: string, "glob"?: string, "mode"?: "content"|"files", "case_insensitive"?: bool, "context"?: int 0..=10}`
Regex search honouring `.gitignore` (ripgrep library crates, as the donor). `mode:"content"`
(default): `<path>:<line>:<text>` grouped by file. `mode:"files"`: matching file paths only;
with `pattern:""` and a `glob` it lists files by glob. No matches → Ok, `No matches.`
Invalid regex → error.

## `shell` — `{"command": string, "timeout_seconds"?: int 1..=3600 (default 120)}`
Runs `bash -lc <command>` with the workspace root as cwd, stdin closed, in its own process
group. Captures stdout+stderr interleaved. On timeout or cancellation the WHOLE process group
is killed (SIGTERM, then SIGKILL after 2 s) — no orphans. Content:
`<bounded output>\n[exit code: <n>]`, or `[timed out after <s> s]`, or status `Cancelled`.
Non-zero exit is `ToolStatus::Ok` (the command ran; the model reads the code). The timeout is a
tool parameter the MODEL chooses — not a harness-imposed limit on the agent.

## `apply_patch` (GPT family) — freeform, `ToolInput::Text`
Declaration kind `Freeform` with the V4A lark grammar. Input:
```
*** Begin Patch
*** Add File: <path>          (+lines)
*** Delete File: <path>
*** Update File: <path>       [*** Move to: <new path>]
@@ [context header]
 context line / -removed / +added
*** End Patch
```
All paths confined; the patch is validated completely
before anything is written (all hunks must locate, in order, with the donor-Codex fuzz
ladder: exact → trailing-whitespace-insensitive → whitespace-insensitive); then applied
atomically per file. Any failure → nothing written, error names the file and the hunk.
Success: one line per file `A|M|D <path>`. The same implementation also offers a function
face `{"patch": string}` for routes without freeform tools.
