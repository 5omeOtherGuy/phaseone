# Tool modules — specification

Every tool is its own crate implementing `p1_contracts::Tool`. A tool gets what it needs
from its constructor (workspace, limits, shared file-observation state); there is no
tool-state bag and no global. Tools contain no provider wire formats and no UI types.
Bodies are extracted from iris-agent (`src/tools/`), adapted to these contracts.

## Crates

| Crate | Tool(s) | Effect | Donor |
|---|---|---|---|
| `p1-workspace` (helper library, not a tool) | path confinement, atomic write, observed-file registry, output bounding | — | `tools/path.rs`, `tools/text.rs`, `ObservedFiles` in `tools/mod.rs`, atomic write in `tools/write.rs`/`edit.rs` |
| `p1-tool-read` | `read` | `ReadOnly` | `tools/read.rs` (with `skim`, without skill roots) |
| `p1-tool-edit` | `edit` | `WritesFiles` | `tools/edit.rs` |
| `p1-tool-write` | `write` | `WritesFiles` | `tools/write.rs` |
| `p1-tool-search` | `grep` | `ReadOnly` | `tools/grep.rs` (+ the `find` glob listing as mode `files`) |
| `p1-tool-shell` | `shell` | `Executes` | `tools/bash/mod.rs` one-shot path only (no sessions, jobs, sandbox) |
| `p1-tool-patch` | `apply_patch` | `WritesFiles` | new (V4A patch format); shares `p1-workspace` |
| `p1-tool-read-output` | `read_output` | `ReadOnly` | iris `src/tools/read_output.rs` (byte cursor instead of lines; ADR-0109) |

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

    /// Read side of the `workspace` capability: the thinnest native surface the
    /// `workspace`/`snapshot` host imports call. `stat`/`list` look at the leaf
    /// WITHOUT following it, so a symlink is reported as a symlink.
    pub fn check_path(&self, requested: &str) -> Result<CheckedPath, WorkspaceError>;
    pub fn stat(&self, requested: &str) -> Result<Stat, WorkspaceError>;
    pub fn list(&self, requested: &str) -> Result<Vec<DirEntry>, WorkspaceError>;
    /// Read the whole file once, record the observation in `observed` and return an
    /// immutable snapshot of exactly those bytes (the mutation side stays elsewhere).
    pub fn read(&self, requested: &str, observed: &ObservedFiles) -> Result<Snapshot, WorkspaceError>;
}
pub enum WorkspaceError {
    NotADirectory(PathBuf),        // `new`/`list` target, or a `read` of a directory, is not a directory
    NotFound{requested},           // resolves inside the root but does not exist
    OutsideWorkspace{requested},   // `..`, absolute, or symlink escape
    Io{path, source},
}

/// A confined path plus its root-relative display form (from `check_path`).
pub struct CheckedPath;
impl CheckedPath {
    pub fn path(&self) -> &Path;    // inside the root after symlink resolution
    pub fn display(&self) -> &str;
}
pub enum FileKind { File, Directory, Symlink, Other }
pub struct Stat { pub kind: FileKind, pub size: u64 }
pub struct DirEntry { pub name: String, pub kind: FileKind }             // name within its directory, never a path

/// One file's bytes as `Workspace::read` found them. Immutable: a later write to the
/// file changes the file, never the snapshot. The whole file is held in memory.
pub struct Snapshot;
impl Snapshot {
    pub fn metadata(&self) -> SnapshotMetadata;                          // path, size, content hash
    pub fn read(&self, offset: usize, len: usize) -> &[u8];              // byte range; past the end returns what exists
}
pub struct SnapshotMetadata { pub path: String, pub size: u64, pub content_hash: u64 }

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

/// One writer at a time among the agents that SHARE it (a parent and its workers; wired by
/// `p1-assembly`, one per catalog). Every mutating file tool holds it from reading the file's
/// current contents until its write is recorded, so the staleness check and the write are one
/// step across agents (ADR-0032). Not held by `shell`.
pub struct WriteGate;
impl Workspace {
    pub fn with_write_gate(self, gate: WriteGate) -> Self;
    pub fn begin_mutation(&self) -> Mutation<'_>;           // guard; synchronous, blocking-thread only
}

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
- `describe(call)` (ADR-0057) returns the tool's own `CallDescription { verb, target, edit }`:
  a short UI verb and trimmed target, plus an optional `EditPreview { path, old, new }` for a
  single-file edit. The tool parses its own input; no argument keys leave the tool.
- The host and UI consume this description so a renamed face changes nothing. The default uses
  verb `call`, puts the declaration name in `target`, and has no edit preview.

## `read`  — `{"file_path": string, "offset"?: int>=1 (default 1), "limit"?: int>=1 (default 2000), "skim"?: bool (default false)}`
Only `file_path` is required; `offset`, `limit` and `skim` have no upper bound in the schema
(the output bound applies). The schema is closed (`additionalProperties: false`).
Returns lines `offset..offset+limit` formatted `<line number right-aligned to 6>\t<text>`
(donor format). Records the FULL file contents as observed. Errors: missing file, directory,
binary file (contains NUL in the first 8 KiB: `<path> is a binary file.`), outside workspace.
When more lines remain: final line `[<n> more lines; continue with offset=<next>]`.
`skim: true` hides comments, docstrings and blank lines of source files, keeping original line
numbers (window and footer count them), and records NO observation: a skimmed read never
authorizes an `edit` or `write`, so a change after a skim is refused like a change to an unread
file until the file is read in full (a skim that falls back to the full window observes nothing
either). A skim that hid lines ends with `[skim: <n> lines hidden; read the file in full before
editing it]` (test `a_change_after_a_skimmed_read_is_refused_like_an_unread_file`). Literals
kept: the lines of a string
literal that can span lines (Rust, C++ raw, Java text block, JS/TS template, Go raw, Python
triple-quoted value, Ruby and shell heredoc) are code and are never hidden (#509). Data or unknown types, an
emptied window or a skim that is not smaller fall back to the full window plus one `[skim: …; showing the full read]` line.
Empty file: `<path> is empty.` only after reading zero bytes from the opened file.
The host capability also limits each opened snapshot to 8 MiB + one detection byte,
checks protected inode identity and protected-directory freshness on the opened handle,
and refuses a concurrently grown file before allocating beyond that limit.
The component currently refuses files above 8 MiB before buffering for observation;
this limit is removed when the snapshot capability can accept a streaming observation
(ADR-0101 records the interface change).

## `edit` — `{"file_path": string, "old_string": string (minLength 1), "new_string": string, "replace_all"?: bool (default false)}`
Required: `file_path`, `old_string`, `new_string`; four properties, schema closed. The model-facing
description and the `old_string` description say: matched exactly first, then a whitespace- and
Unicode-tolerant fallback that echoes the applied region (ADR-0106, issue #505; test
`the_description_names_the_tolerant_fallback`). String replacement. `old_string == new_string` → error. 0 matches → error
`old_string was not found in <path>.` >1 matches without `replace_all` → error
`old_string occurs <n> times in <path>; add context to make it unique or set replace_all.`
Preserves untouched bytes, including each mixed line ending, plus the trailing newline. Atomic write. Success content:
`Edited <path> (<n> replacement(s)).`
When the exact match finds nothing, a folded match is tried (Unicode spaces, curly quotes
and Unicode dashes as their ASCII form, trailing whitespace at the end of a line dropped;
indentation and every other character still exact) and a unique folded match is applied with
`\nApplied region (tolerant match):\n<numbered region>` appended (ADR-0106); a region over 40
lines or 8,000 bytes shows its first and last lines around `     … <n> lines not shown` (#509). A 0-match error
appends `\nClosest matching region (around line <n>):\n<numbered region>`, bounded to 200
characters a line and ±2 lines.

## `write` — `{"file_path": string, "content": string}`
Creates or replaces a file atomically (parents created). Existing target → read-before-mutate
applies. Native queued calls recheck cancellation after acquiring the write gate and before
replacing the file. Success: `Wrote <path> (<bytes> bytes).`

## `grep` — `{"pattern": string, "path"?: string, "glob"?: string, "mode"?: "content"|"files"|"count", "case_insensitive"?: bool, "literal"?: bool, "context"?: int 0..=10, "offset"?: int >=0, "head_limit"?: int >=1, "max_per_file"?: int >=1}`
Only `pattern` is required; ten properties, schema closed. Defaults: `path` the workspace root,
`glob` none, `mode` `"content"`, `case_insensitive` false, `literal` false, `context` 0,
`offset` 0, `head_limit` none (no paging cut), `max_per_file` none (no per-file cap).
Regex search honouring `.gitignore` (ripgrep library crates, as the donor). `mode:"content"`
(default): grouped by file — one block per file, the path on its own line, then `<line>:<text>`
for a match and `<line>-<text>` for a context line, blocks separated by a blank line, files in
bytewise path order. `mode:"files"`: matching file paths only; with `pattern:""` and a `glob`
it lists files by glob. No matches → Ok, `No matches.` Invalid regex → error. `literal: true`
(default false) matches `pattern` as exact text: the guest escapes it before the host's regex
search (#509).
Native calls cancel a running walk when the call's cancellation token is cancelled, not only
when the returned future is dropped.
**Bounding (research #36).** A result over the shared output bound is cut by `grep` itself, never
mid-block: whole file blocks (in `files` mode: whole paths) are kept while they fit, and the
footer says what is missing and how to get it:
`[truncated after <last path shown>; <n> more matching files not shown; narrow with path or glob]`.
If even the first block does not fit, that block is cut at a line boundary and the footer reads
`[truncated inside <path> after line <line>; <n> more matching files not shown; narrow with path, glob or a stricter pattern]`.
Content searches stream the walk in bounded chunks (at most 4,096 paths / 512 KiB per
chunk, each sorted by displayed path and merged into one bytewise-ordered result) and
keep only bounded match lines plus omitted counts, even in large workspaces; a single very
wide directory is therefore never collected whole. `mode:"files"` with an empty pattern
still requires a file listing; until the workspace interface supports pagination, that
listing refuses
above 4,096 paths or 512 KiB retained path names and asks to narrow path/glob
(ADR-0101), while a patterned `mode:"files"` search keeps the paths its first bounded
search carried and counts the rest.
**Count, paging and per-file cap (issue #493, donor `tools/grep.rs`, `tools/find.rs`).**
`mode:"count"`: one `<path>:<n>` line per matching file, then `[total: <m> matches in <f> files]`,
exact past the line cap (the walk resumes file by file). `<n>` and `<m>` count MATCHING LINES,
not occurrences: a line with two hits counts once. Example, copied from the unit test
`count_lines_page_and_describe` (`crates/p1-tool-search/logic/src/lib.rs`):
```
a.rs:3
b.rs:1
c.rs:2
[total: 6 matches in 3 files]
``` A resumed file search carries at most
128,000 lines; a file past that counts as a lower bound (`<n>+`, and `… at least <k> more` or
`… matches after line <n> not searched` in content mode).
`offset` skips that many output entries and `head_limit` keeps at most that many; an entry is
a match line (`content`), a path (`files`) or a count line (`count`). When entries remain the
page footer `[showing <matches|files> <a>-<b>[ of <total>]; continue with offset=<b>]` follows
the entries (in `count` mode after the total line); it is not always the last line: on a
`files` page the directory summary below follows it, and in every mode the note
`[<n> more matching files not searched; narrow with path or glob]` comes last when the walk
could not reach some matching files. Example, copied from the unit test
`a_files_page_shows_files_three_to_five_and_names_offset_five`
(`crates/p1-tool-search/logic/src/lib.rs`):
```
f3
f4
f5
[showing files 3-5 of 7; continue with offset=5]
[7 matching files, 3 shown, 4 omitted; omitted by directory: ./ (4)]
```
An offset past the end reads `[showing no <noun>: offset <n> is past the last of <total>]`.
A page cut by the byte bound drops its first match's leading context before it cuts or omits
that match, so following the named offsets shows every entry once (#509).
`max_per_file` (content mode) shows a file's first N matches, then `… <k> more matches in this
file`. A paged `files` result that leaves paths out (by `head_limit` or the output bound) follows
its footer with `[<total> matching files, <shown> shown, <omitted> omitted; omitted by directory: <top 5>]`.
A call that names none of these (or only `offset:0`) renders exactly as above; the summary is
therefore not added to an unpaged `files` result, whose footer the acceptance tests fix.

## `shell` — `{"command": string, "timeout_seconds"?: int 1..=3600 (default 120)}`
Runs `bash -lc <command>` with the workspace root as cwd, stdin closed, in its own process
group. Captures stdout+stderr interleaved. On timeout or cancellation the WHOLE process group
is killed (SIGTERM, then SIGKILL after 2 s) — no orphans. A watchdog enforces the timeout even when a guest stops polling its process stream; it records whether the group was still running when the deadline passed, so a command that had already finished keeps its own exit and only a group alive at the deadline is `TimedOut`. The leader's exit is observed alongside pipe reads, so a background child inheriting the pipes does not defer group cleanup until the timeout; if cleanup of a still-running group crosses the timeout the run ends `TimedOut`, not successfully. Dropping a process resource sends SIGKILL to its group synchronously, then reaps its leader in the background; explicit cancellation still waits for group termination. The GROUP is watched, not the
shell: whatever is left of it after the grace period is killed even if the shell itself exited
at once, and the tool returns when the group is empty or the bounded post-SIGKILL wait expires.
Every wait in termination is bounded, including the leader's own reap: a leader stuck in
uninterruptible kernel work cannot make timeout, cancellation or kill hang. While the
leader is still running it is NOT reaped before the final signal: a live leader, or the
zombie `waitid(WNOWAIT)` leaves behind, reserves the group ID, so SIGTERM/SIGKILL cannot
land on an unrelated group whose ID was reused. Content:
`<bounded output>\n[exit code: <n>]`, or `[timed out after <s> s]`, or status `Cancelled`.
Non-zero exit is `ToolStatus::Ok` (the command ran; the model reads the code). The timeout is a
tool parameter the MODEL chooses — not a harness-imposed limit on the agent.
Stored output (ADR-0109, #511): the host stores every command's output, masked, before it cuts
it. When the result shows less than the command printed — a filter summarised it, or the host's
head/tail capture or the 50,000-byte bound cut it, or the store stopped early so it cannot tell —
one line just above the end footer names the
stored output: `[stored output: handle_id <h>, <n> bytes, <state>; page it with read_output]`,
where `<state>` is `complete`, `stopped at the store's cap, later output not stored` or
`incomplete, the store fell behind and later output was not stored`. When the store could not
write, the line is `[full output not stored; recovery unavailable]` and no handle is shown. The
line is part of the footer the byte bound never cuts, filtered or `raw: true`; a result that shows
the whole output carries no line.

### `shell` output filters (research #42, donor `iris-agent` `src/tools/bash/filter/`)

The schema is `{"command": string, "timeout_seconds"?: int 1..=3600 (default 120), "raw"?: bool
(default false)}`; only `command` is required and the schema is closed. `raw` is the third property (default false). With `raw` false, the captured output of a
RECOGNISED command is summarised after the command exits and before the byte bound: passing
`cargo test`/`cargo build`/`cargo check`/`cargo clippy` logs lose their progress lines and keep
results, warnings and errors; `git status`, `git log`, `git diff` and `npm`/`pnpm test` get the
donor's summaries. Behind them, the donor's declarative tier also runs: 62 of its 64 TOML filter
files (source: RTK, Apache-2.0, vendored by iris-agent; a few are iris-authored — see `data/NOTICE.md`), converted once to JSON
(`crates/p1-tool-shell/guest/src/filter/data/*.json`) so the guest keeps its serde, serde_json and
regex dependencies only (ADR-0081), cover the long tail of tool classes (`make`, `helm`,
`terraform`/`tofu`, `pulumi`, cloud CLIs, linters). `npm-install` and `shellcheck` are left out:
the frozen filter corpus (`crates/p1-tool-shell/tests/output_filters.rs`) requires those classes to pass through raw. The files are embedded with
`include_str!` — the guest is WebAssembly and has no filesystem — parsed once into a `OnceLock`
registry, and applied by the donor's eight-stage engine (ANSI strip, `replace`, `match_output`
short-circuit, strip/keep lines, line truncation, head/tail, `max_lines`, `on_empty`). A
declarative filter applies only when no structured filter matched; a structured decline never falls through to
one. Recognition works on the effective command (a leading `cd <path> &&`, env
assignments and wrappers are looked through, as the donor's `effective_command` does). The
declarative tier takes a compound command only when its last segment alone produces the
output: earlier segments must be silent by form (`cd <dir>`, `export NAME=value`) or bare assignments
joined by `&&`, `;` or a newline; a pipe, `||`, a background `&`, a subshell that prints or a
printing earlier segment makes the shape ambiguous, and the tier DECLINES: the output stays raw
(ambiguity-decline rule, #509; test `an_ambiguous_shell_shape_declines_the_declarative_tier`). Its patterns end a program name at a blank or the end
(`(?:\s|$)`, not `\b`), so `ssh-keygen` or `helm-docs` select no filter (#507, #509).
Fail-safe contract — every line is a test:
- an unrecognised command, a filter that declines, errors or panics, a filter result that is not
  SHORTER than its input, and a filter that empties non-empty output all yield the RAW output;
- the output of a FAILING command (non-zero exit) keeps every error and failure line verbatim;
- the `[exit code: <n>]` / timeout footer is appended after filtering and is never touched;
- `raw: true` bypasses filtering entirely; the tool description says so in one sentence, and a
  filtered result ends with the line `[output filtered; pass raw:true for the full log]`;
  host head/tail capture may already have omitted middle bytes even in raw mode. In that
  case the omission marker explicitly warns that diagnostics may be missing, and that marker
  line survives every summary;
- the head/tail byte bound stays as the backstop after the filter.
Structured filters are pure `(&str, bool) -> Option<String>` functions in a private module of
`p1-shell-guest`: no provider or UI type, no global registry, no build script, no usage
accounting. The declarative tier keeps the same fail-safe contract: an error-guard regex keeps
error/failure lines (make's `*** ... Error N` included) from being stripped, the lossy stages and a success-flavored `on_empty` are
skipped on a non-zero exit, and a filter that empties non-empty output yields the RAW output. One
test runs every inline `tests.<name>` case of all 62 files; another proves every file parses
and every definition compiles.

### `shell` environment — an allow-list, always

A command does NOT inherit the environment of the p1 process. The user's shell usually exports
API keys and tokens, and agent sockets are named by variables (`SSH_AUTH_SOCK`,
`DBUS_SESSION_BUS_ADDRESS`); none of that is the agent's business. The child's environment is
cleared and rebuilt from an allow-list, sandboxed or not:
- by exact name: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`, `TERM`, `TZ`,
  `COLORTERM`, `NO_COLOR`, `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `RUSTFLAGS`,
  `CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS`, `P1_BUILD_LOCK_DIR`, `P1_RUSTC_SLOTS`,
  `VIRTUAL_ENV`, `NVM_DIR`, `JAVA_HOME`, `GOPATH`, `GOROOT`;
- by prefix: `LC_`;
- plus the names the host was given with repeatable `--env-pass NAME` (a name containing `=`
  or an empty name is a usage error, exit 2).
`ShellTool::with_env_pass(self, names: Vec<String>) -> Self` adds to the list; the built-in
list is `pub const ENV_ALLOW: &[&str]` and `pub const ENV_ALLOW_PREFIXES: &[&str]`. The
environment is read from an injected snapshot (`ShellTool::with_env_snapshot(Vec<(OsString, OsString)>)`,
default: the process environment at construction) so tests never mutate the process
environment. In the sandbox `TMPDIR=/tmp` is still set by bubblewrap, after the allow-list.
Stated limit: `bash -lc` is a login shell; WITHOUT the sandbox it sources the user's profile,
which may export secrets again, and the files are readable anyway — the allow-list is a
boundary only together with the sandbox (which hides the profile files).
Must-pass: a snapshot with `CANARY_TOKEN=secret-1`, `SSH_AUTH_SOCK=/x`, `PATH`, `LC_ALL=C`,
`MY_TOOL_HOME=/opt/t`: `env` inside the command shows no `CANARY_TOKEN`, no `SSH_AUTH_SOCK`,
shows `PATH` and `LC_ALL`; `MY_TOOL_HOME` only with `with_env_pass(["MY_TOOL_HOME"])`; the same
through the sandbox (real bwrap, SKIP rule as for the other sandbox tests); through the host
with `--env-pass MY_TOOL_HOME`; `--env-pass A=B` exits 2.

### `shell` sandbox — the execution boundary (optional; the HOST chooses it)

Authorization decides WHETHER a command may run; it cannot constrain what a running command
does. The sandbox is that constraint: with it, a `shell` command can write only inside the
workspace and a private `/tmp`, and sees of the user's home only what is listed. It uses
bubblewrap (`bwrap`, unprivileged user namespaces); nothing is installed or run as root.

```rust
pub struct Sandbox {
    pub home: PathBuf,               // the home directory to hide
    pub home_visible: Vec<PathBuf>,  // RELATIVE to `home`; visible read-only if they exist
    pub readable: Vec<PathBuf>,      // existing absolute paths; bound READ-ONLY unless already under a writable root
    pub writable: Vec<PathBuf>,      // extra absolute paths that stay writable if they exist
    pub runtime_dir: Option<PathBuf>, // e.g. $XDG_RUNTIME_DIR: hidden behind a tmpfs (agent sockets, keyrings)
}
impl Sandbox {
    /// `home_visible` = the entries of `DEFAULT_HOME_VISIBLE` — `.cargo`, `.rustup`,
    /// `.local/bin`, `.local/lib`, `.nvm`, `.gitconfig`, `.config/git` — nothing else.
    pub fn for_home(home: impl Into<PathBuf>) -> Self;
}
pub const CREDENTIAL_DIRECTORIES: &[&str]; // `.ssh`, `.claude`, `.codex`, `.gnupg`,
                                          // `.local/share/opencode`, `.pi`, `.config/gh`, `.config/p1`
pub enum SandboxError { NotInstalled, UnsafeLauncher, Unavailable(String), WorkspaceContainsHome, WritableAncestor { path: PathBuf, root: PathBuf }, ReadableUnresolved { path: PathBuf }, ReadableCredential { path: PathBuf, directory: PathBuf } }
impl ShellTool {
    /// Probes ONCE (`bwrap <args> true`), so an unusable sandbox fails assembly, not the
    /// first command. `NotInstalled`: no `bwrap` on PATH. `Unavailable(stderr)`: it cannot
    /// run here (user namespaces disabled). `WorkspaceContainsHome`: the workspace root is
    /// the home directory or an ancestor of it — hiding the home would hide the workspace.
    /// `WritableAncestor`: a writable path may not contain the hidden home, workspace,
    /// or private `/tmp` (lexically or canonically).
    /// `ReadableUnresolved`: a `readable` path must exist and resolve. A source already
    /// under a writable root needs no extra bind. `ReadableCredential`: any bind source
    /// equal to, inside or an ancestor of a credential directory is refused, both lexically
    /// and canonically, even when it does not exist.
    pub fn sandboxed(self, sandbox: Sandbox) -> Result<Self, SandboxError>;
}
// `UnsafeLauncher` is raised at assembly when PATH has executable candidates but
// all are unsafe, or at command start as `ProcessFailure::Start` if the cached launcher changes.
/// Pure, unit-tested: the argument vector before `bash -lc <command>`.
pub fn bwrap_args(sandbox: &Sandbox, workspace_root: &Path, private_tmp: &Path) -> Result<Vec<OsString>, SandboxError>;
```

The command becomes `bwrap <args> bash -lc <command>`; process group, timeout, cancellation,
output capture and footers are exactly those of the unsandboxed tool. Mount plan — the ORDER
is part of the contract, because a later mount covers an earlier one and a workspace may
itself live under `/tmp` or under the home:
1. `--ro-bind / /`, `--dev /dev`, `--proc /proc`;
2. `--bind <private_tmp> /tmp` — a fresh directory per `ShellTool`, created under
   `std::env::temp_dir()` and removed when the tool is dropped; `--setenv TMPDIR /tmp`;
3. `--tmpfs <home>` (`<home>` CANONICAL — the same path the containment check used), then
   bind each safe existing `home_visible` entry and configured `readable` path read-only.
   Skip sources already under the workspace, a writable path, or private `/tmp`; recreate
   an alias with `--symlink <canonical> <configured-path>` when the home tmpfs hid its
   destination: a lexical parent under home (even when it resolves into a writable
   root) or a parent that canonicalizes into home without an enclosing writable bind.
   Outside home, the root bind exposes it.
   Other sources use `--ro-bind <canonical-source> <configured-path>`;
   `--tmpfs $XDG_RUNTIME_DIR` when that
   variable names an existing directory — agent sockets and keyrings live there;
4. `--bind <canonical-source> <path>` for nonredundant existing `writable` paths; nested,
   duplicate or missing entries under writable roots get no bind; aliases get `--symlink`;
5. `--bind <workspace root> <workspace root>`;
6. AFTER readable, writable and workspace binds, mask existing cargo credentials with
   `--ro-bind /dev/null <home>/.cargo/credentials.toml` (and `…/credentials`), so no later
   bind can uncover a token;
7. `--remount-ro <home>` — writes to the hidden home fail loudly (`Read-only file system`)
   instead of vanishing into a tmpfs;
8. `--unshare-pid --die-with-parent --chdir <workspace root>`.
The network stays shared (fetching dependencies is normal work). The PID namespace closes the
hole the unsandboxed tool has: a process that leaves the process group (`setsid`, double
fork) still dies with the sandbox when the command is cancelled or times out.

Model-facing: the description gets one more paragraph — `Commands run in a sandbox: only the
workspace and /tmp are writable, the rest of the filesystem is read-only, and most of the home
directory is not visible. Do not try to install software outside the workspace.` — and the
identity variant becomes `<variant>+sandbox`.

Host: `--sandbox workspace|off` (default `off` in this increment; the default is revisited
after dogfooding), repeatable `--sandbox-write PATH` and repeatable `--sandbox-read PATH`
(each a usage error without `--sandbox workspace`). The sandbox applies to the parent's AND
every worker's `shell`. `scripts/fanout.py` and `scripts/dogfood.sh` pass the git common
directory as `--sandbox-read` when the job dir is a git WORKTREE, so the agent can inspect
(never commit) the metadata that lives in the main checkout. Assembly-time `SandboxError`
returns exit 1 before any model call; `UnsafeLauncher` instead refuses a command start as
`ProcessFailure::Start`. Each error names a remedy.

Must-pass (real `bwrap`; a test returns early with a printed `SKIP: bwrap unusable here` when
the probe fails — CI runners may forbid user namespaces): fake home `H` containing
`.secret/token`, `.cargo/bin/`, and the workspace `H/ws`; (a) `echo x > a.txt` works and the
file exists on the host; (b) `echo x > ../outside.txt` fails, exit code ≠ 0, nothing created
on the host; (c) `cat ~/.secret/token` (with `HOME=H`) fails and the content never appears in
the output; (d) `ls H/.cargo` works, `touch H/.cargo/z` fails; (e) `echo t > /tmp/t` works and
the host's real temp dir has no `t`; (f) a path listed in `writable` is writable; (g)
`setsid sleep 300 & echo $! > pid; wait` cancelled → that pid is gone when the tool returns
(PID namespace numbers differ: have the child write its HOST-visible identity another way —
e.g. `sleep 300` with a unique argument such as `sleep 300.0731`, then look for that command
line in `/proc/*/cmdline` on the host); (h) workspace == home → `WorkspaceContainsHome`;
(i) `bwrap_args` order exactly as listed, for a workspace under `/tmp` and one under the home;
(j) a `readable` path inside the hidden home can be read but not written, a `readable`
path equal to, inside or an ANCESTOR of a `CREDENTIAL_DIRECTORIES` entry (the home itself,
`~/.config`, `~/.local/share`, `/`, an ancestor of the home, a symlink to any of those) is
refused with an error naming the path and the directory, and a safe path (a worktree common
dir, `~/.config/git`, `~/.cargo`) stays accepted; (k) in a scratch repo + worktree under a
scratch HOME, `git status --porcelain` works inside the sandbox when the worktree's common
dir is `readable`, and `git commit` fails.

## `read_output` — `{"handle_id": string (minLength 1), "offset"?: int 0..=u64::MAX (default 0; null is invalid), "limit"?: int 1..=50000 (default 50000), "pattern"?: string (minLength 1), "literal"?: bool (default false)}`
Pages an output the host stored (ADR-0109) through the `tool-outputs` capability, the
`p1/read-output` component's only grant. `offset` is a zero-based UTF-8 byte cursor; the schema
is closed. Content: the page text, a line break, then one footer line
`[read_output: bytes <offset>-<next> of <stored> stored; capture <state>; next_offset <next>]`, or
`...; end]` on the last page (an empty page at the end is the footer alone). A page never splits a
character, so the page texts put together are the stored text byte for byte. Errors are `Error`
outcomes with their own text: an unknown handle (another run's, another session's, a malformed
one, or one whose capture failed), a limit smaller than the character at the offset (never an
empty page with the same cursor), an offset past the end (naming the stored bytes), an offset
inside a character, and a read failure in the host's words.

With `pattern` (#526) the call searches instead of paging: a regular expression (Rust `regex`
syntax), or exact text with `literal: true` as in `grep`; `literal` without `pattern` and `limit`
with `pattern` are invalid input, and so is a pattern that does not compile. The guest scans the
output from `offset` (default 0) line by line (a line ends at `\n`; a `\r` before it is not part
of the line), reading the store through the same `tool-outputs` `page` call in 1 MiB pages, so the
interface is unchanged. Content: one line `<offset>: <line>` per matching line, `<offset>` the
byte offset the line starts at (a later `read_output` from it pages from that line), the line cut
to 200 characters with `…` marking a cut, then one footer line
`[read_output: <n> matches in bytes <offset>-<scanned> of <stored> stored; capture <state>; end]`.
The scan stops early, naming the offset to continue from instead of `end`, at 30 matches
(`match limit 30 reached, continue with offset <next line>`) or after a scan budget of 16 MiB
read from the store in one call, the store's default per-output cap
(`scan budget 16777216 bytes reached, continue with offset <next unscanned line>`); a single line
longer than the budget is matched as far as it was read and the scan continues after that.
No match is an `Ok` result with `0 matches`. Reading runs no command: the tool
records no command evidence and never counts as a verification run for `finish`. Environments
list it wherever they list `shell`.

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
# Result descriptions and destructiveness (ADR-0059)

Tools describe their own execution results through `Tool::describe_result`:
the summary is the result's first line by default, while `ResultDetail` can
carry a diff, command facts, matches, files, or text. The host renders these
descriptions without decoding a tool's private input or output.

`CallDescription.destructive` is the tool's pre-execution judgement. The host
uses it to show the destructive approval floor and disable persistent grants;
the default is `false`.
