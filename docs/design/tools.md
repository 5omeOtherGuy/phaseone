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

## `read`  — `{"file_path": string, "offset"?: int>=1 (default 1), "limit"?: int>=1 (default 2000)}`
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
(default): grouped by file — one block per file, the path on its own line, then `<line>:<text>`
for a match and `<line>-<text>` for a context line, blocks separated by a blank line, files in
bytewise path order. `mode:"files"`: matching file paths only; with `pattern:""` and a `glob`
it lists files by glob. No matches → Ok, `No matches.` Invalid regex → error.
**Bounding (research #36).** A result over the shared output bound is cut by `grep` itself, never
mid-block: whole file blocks (in `files` mode: whole paths) are kept while they fit, and the
footer says what is missing and how to get it:
`[truncated after <last path shown>; <n> more matching files not shown; narrow with path or glob]`.
If even the first block does not fit, that block is cut at a line boundary and the footer reads
`[truncated inside <path> after line <line>; <n> more matching files not shown; narrow with path, glob or a stricter pattern]`.
The schema does not change.

## `shell` — `{"command": string, "timeout_seconds"?: int 1..=3600 (default 120)}`
Runs `bash -lc <command>` with the workspace root as cwd, stdin closed, in its own process
group. Captures stdout+stderr interleaved. On timeout or cancellation the WHOLE process group
is killed (SIGTERM, then SIGKILL after 2 s) — no orphans. The GROUP is watched, not the
shell: whatever is left of it after the grace period is killed even if the shell itself exited
at once, and the tool returns only when the group is empty. Content:
`<bounded output>\n[exit code: <n>]`, or `[timed out after <s> s]`, or status `Cancelled`.
Non-zero exit is `ToolStatus::Ok` (the command ran; the model reads the code). The timeout is a
tool parameter the MODEL chooses — not a harness-imposed limit on the agent.

### `shell` output filters (research #42, donor `iris-agent` `src/tools/bash/filter/structured/`)

The schema gains `"raw"?: bool` (default false). With `raw` false, the captured output of a
RECOGNISED command is summarised after the command exits and before the byte bound: passing
`cargo test`/`cargo build`/`cargo check`/`cargo clippy` logs lose their progress lines and keep
results, warnings and errors; `git status`, `git log`, `git diff` and `npm`/`pnpm test` get the
donor's summaries. Recognition works on the effective command (a leading `cd <path> &&`, env
assignments and wrappers are looked through, as the donor's `effective_command` does).
Fail-safe contract — every line is a test:
- an unrecognised command, a filter that declines, errors or panics, a filter result that is not
  SHORTER than its input, and a filter that empties non-empty output all yield the RAW output;
- the output of a FAILING command (non-zero exit) keeps every error and failure line verbatim;
- the `[exit code: <n>]` / timeout footer is appended after filtering and is never touched;
- `raw: true` bypasses filtering entirely; the tool description says so in one sentence, and a
  filtered result ends with the line `[output filtered; pass raw:true for the full log]`;
- the head/tail byte bound stays as the backstop after the filter.
Filters are pure `(&str, bool) -> Option<String>` functions in a private module of
`p1-tool-shell`: no provider or UI type, no global registry, no build script, no usage
accounting. The donor's declarative TOML engine and its vendored third-party filter files are
NOT taken. `regex` becomes a direct dependency of `p1-tool-shell` (already in the lock tree).

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
    pub readable: Vec<PathBuf>,      // extra absolute paths visible READ-ONLY if they exist
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
pub enum SandboxError { NotInstalled, Unavailable(String), WorkspaceContainsHome, ReadableCredential { path: PathBuf, directory: PathBuf } }
impl ShellTool {
    /// Probes ONCE (`bwrap <args> true`), so an unusable sandbox fails assembly, not the
    /// first command. `NotInstalled`: no `bwrap` on PATH. `Unavailable(stderr)`: it cannot
    /// run here (user namespaces disabled). `WorkspaceContainsHome`: the workspace root is
    /// the home directory or an ancestor of it — hiding the home would hide the workspace.
    /// `ReadableCredential`: a `readable` path is equal to, inside or an ANCESTOR of a
    /// `CREDENTIAL_DIRECTORIES` entry of the home (or of the home itself) — refused before
    /// the probe, whether or not that directory exists yet.
    pub fn sandboxed(self, sandbox: Sandbox) -> Result<Self, SandboxError>;
}
/// Pure, unit-tested: the argument vector before `bash -lc <command>`.
pub fn bwrap_args(sandbox: &Sandbox, workspace_root: &Path, private_tmp: &Path) -> Vec<OsString>;
```

The command becomes `bwrap <args> bash -lc <command>`; process group, timeout, cancellation,
output capture and footers are exactly those of the unsandboxed tool. Mount plan — the ORDER
is part of the contract, because a later mount covers an earlier one and a workspace may
itself live under `/tmp` or under the home:
1. `--ro-bind / /`, `--dev /dev`, `--proc /proc`;
2. `--bind <private_tmp> /tmp` — a fresh directory per `ShellTool`, created under
   `std::env::temp_dir()` and removed when the tool is dropped; `--setenv TMPDIR /tmp`;
3. `--tmpfs <home>` (`<home>` CANONICAL — the same path the containment check used), then
   `--ro-bind <home>/<entry> <home>/<entry>` for every existing `home_visible` entry, then
   `--ro-bind <path> <path>` for every existing `readable` path (e.g. a git worktree's
   common directory, from `--sandbox-read`); `--tmpfs $XDG_RUNTIME_DIR` when that variable
   names an existing directory — agent sockets and keyrings live there;
4. `--bind <path> <path>` for every existing `writable` path; THEN the masks, so that no
   writable bind can uncover them: `--ro-bind /dev/null <home>/.cargo/credentials.toml` (and
   `…/credentials`) if that file exists — a visible or writable directory must not leak a token.
   The `readable` binds come BEFORE this step, so no readable path can uncover
   `~/.cargo/credentials*`;
5. `--bind <workspace root> <workspace root>`;
6. `--remount-ro <home>` — writes to the hidden home fail loudly (`Read-only file system`)
   instead of vanishing into a tmpfs;
7. `--unshare-pid --die-with-parent --chdir <workspace root>`.
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
(never commit) the metadata that lives in the main checkout. A `SandboxError` fails assembly
— exit 1 before any model call — with a message that names the remedy (`install bubblewrap,
or pass --sandbox off`).

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
