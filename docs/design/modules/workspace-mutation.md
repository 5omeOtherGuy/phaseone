# Workspace capabilities in the host: `workspace`, `snapshot` and `workspace-mutation`

Status: **proposal** by stream S2, for the S1 lead's approval. It is not part of the frozen
boundary indexed in [`README.md`](README.md) and changes nothing there: the interfaces are the
frozen ones of [`modules/wit/workspace.wit`](../../../modules/wit/workspace.wit), and this
document says how the native host implements them. The decision it supports is
[`../adr/0088-workspace-capabilities-across-components.md`](../adr/0088-workspace-capabilities-across-components.md)
("Workspace capabilities across components"), which merges `proposed`.

## Why a proposal is needed

[`wit.md`](wit.md) freezes the three workspace interfaces and names `p1-workspace` as their
owner (freeze item 3, F9), but leaves open how the native service keeps ADR-0025 (confinement
after symlinks, read-before-mutate, content-based staleness, the `apply_patch` exemption) and
ADR-0032 (agents sharing a directory serialize their file mutations) once the file tools are
components. Today each native tool holds the `WriteGate` from reading a file to recording its
write, on a blocking thread, with direct access to the agent's `ObservedFiles`. A component has
neither a thread nor a path nor a registry: it has only the imports its assembly links. The S2
brief also asks for a `commit` step (U-mut) that the frozen WIT does not contain (BLOCKERS.md
S2-B4). This document fixes both without a WIT change.

## Who owns what

| Part | Owner | Where |
|---|---|---|
| `workspace` (`stat`, `read`, `list-files`, `search`) | `p1-workspace`, read side (S1) | `crates/p1-workspace/src/read.rs` and its successors |
| search's read without observation | `p1-workspace`, S2 | beside S1's read side |
| `snapshot` (`observe`, `check`) | `p1-workspace`, read side (S1) | over `ObservedFiles` |
| `workspace-mutation` (`begin`, the `mutation` resource) and `commit` | `p1-workspace`, mutation side (S2) | beside `gate.rs` and `text.rs` |
| linking the interfaces into a component's store | `p1-module-runtime` (S0, then S1) | the loader's linkable set (BLOCKERS.md S2-B3) |
| which assembly gets which grant and mode | `p1-host` catalog, S2's rows | `crates/p1-host/src/catalog/tools.rs` (U-cat) |

The host functions are thin: each one maps its WIT arguments onto a native `p1-workspace` call
and maps `WorkspaceError` onto the frozen `fs-error`. All enforcement lives in the native
service, so the same code serves the native tools that remain and the components that replace
them.

The mapping is fixed, so that a component's error text can be the native tool's byte for byte:
`outside-workspace` and `not-found` are their own variants, `already-exists` comes from the
mutation side, `wrong-kind` is a path that exists but is not the kind the call needs (a
directory asked for as a file), and every other failure is `io(message)`, where `message` is
what the native tool prints for that failure — the `std::io::Error`'s own text, such as
`Not a directory (os error 20)` or `Permission denied (os error 13)` — never a host path and
never the `WorkspaceError`'s own `Display`. A resource method's recheck refusal is the one
`io` message the host words itself rather than taking from the filesystem; it is the native
tool's text too ("You must read … before changing it.", "… changed on disk since you last read
it; read it again.").

## Per-agent state the host binds into a call

Every export call of a tool component runs in a fresh store (ADR-0082). Before the call, the
host puts into that store's data:

- the agent's `Workspace` (its canonical root and its `WriteGate` handle; agents assembled from
  one catalog share one gate, ADR-0032);
- the agent's own `ObservedFiles` (per agent: a parent and each worker keep separate
  registries, ADR-0032);
- the assembly's **mutation mode** from the catalog row: `observed` or `patch-authorized`, or
  none when the assembly does not link `workspace-mutation`;
- an empty **call read record**: keyed by resolved path, the digest of the whole file as it
  was when this call last read it through `workspace.read`. `read` is windowed, so the host
  digests the whole file on every read call and the latest read of a path wins; step 3 then
  compares the target's current whole-file digest with the recorded one, whatever window the
  component asked for. A removed or renamed-away source loses its call read identity.
  Host snapshot reads and mutation inspection refuse files larger than 32 MiB before
  materializing them, with a bounded read to catch growth during the open.
  Mutations refuse credential names and hard-link identities at the opened leaf; source
  and destination of rename get the same refusal. The protected-directory index is
  revalidated under the gate before each leaf is checked, so a hard link added to a
  protected store after the index was captured is refused through its alias too. Under
  the gate, the checked leaf's device, inode and inspected contents are compared again
  through its held parent directory immediately before apply. This catches substitutions
  and same-inode rewrites during staging, not writes by an ungated actor in the last
  interval after that comparison (no atomic leaf CAS is available).
  Stat and directory listing open their objects through no-follow descriptor walks.
  Search's protected-file index opens exact stores by descriptor and enumerates
  protected directories from opened handles (including nested directories), then
  refreshes the index against current policy at each candidate open so a retargeted
  credential directory cannot reuse the previous target's index.

Nothing of this is visible to the guest. The guest has no preopened directory and no descriptor
(guest preopens stay empty: the guest target has no WASI at all, D-XO-4), and it never receives
a host path; every path it names is a workspace-relative or in-root absolute string that the
host resolves again on every call.

## `workspace`

- `stat`, `list-files`, `search` are the confined read side as S1 implements it; none records an
  observation.
- `read(path, offset, length)` reads a window of the file under confinement. Whether it records
  the agent's observation is a property of the assembly, not of the call:
  - **read tool** (`workspace` + `snapshot`): the module records its observation with
    `snapshot.observe` after reading the whole file, as the native read tool does with its
    streamed hash.
  - **search tool** (`workspace` only): reads never record an observation, so a search can never
    grant the permission edit and write check. This is the "read without observation" of the
    brief, and it holds by construction: search does not link `snapshot`, and the only other
    way an observation is recorded is a successful observed mutation, which search does not
    link either.
  - **edit, write and patch**: reads add the file's digest to the call read record (below)
    and record no agent observation. The contents a change wrote are recorded by the host
    (step 6); edit and write still invoke `snapshot.observe` for interface parity,
    but the call-scoped host ignores that import after a successful mutation (the
    host already recorded the opened destination, while a requested symlink could
    retarget before `observe`). Patch links no `snapshot`.

## `snapshot`

`observe(path, contents)` records `contents` as the agent's observation of `path`;
`check(path, current)` compares `current` with it. The host resolves `path` under confinement
first and keys the registry by the resolved path, as `ObservedFiles` does. Staleness stays
content-based (a digest of the bytes, never modification times). Three of the filesystem tools
link `snapshot`: the read tool for its own observations, and edit and write for the reason in
"Error precedence" below — `check` is the only way a component can learn the observation state
before its mutation, and `observe` is what the native tools do after a write. What the grant
moves is who may record an observation: with it, an `edit` or `write` component can record an
observation of a file whose contents that call did not read, exactly as the read tool records
one it did, so the registry keeps describing what an agent has seen of a file. Nothing else
moves: the host still checks every observed mutation in step 3 against the recorded
observation, so the enforcement of ADR-0025 is where it was.

## `workspace-mutation` and `commit`

`begin()` waits for the agent's `WriteGate` and returns a `mutation` resource that holds it.
The wait is an asynchronous host import (the guest is suspended; no blocking thread is taken,
so no `spawn_blocking` sits on a guest path), and the critical section behind it is a few file
operations, never a model or network wait.

Each method of the resource is one call of the native `commit` API of `p1-workspace` with a
single change: `commit` is the native entry point, the `mutation` resource is its only
component-facing form (BLOCKERS.md S2-B4, option a). For one change `commit`:

1. **Validates the target** beneath the workspace: resolution after symlinks exactly as
   `Workspace::resolve`, so `..`, absolute outside paths and escaping symlinks are
   `outside-workspace`.
2. **Holds the gate**: the gate is already held by the `mutation` resource; `commit` never
   takes it again, so it cannot wait on a gate the call itself holds. A second `begin()` in
   one export call, while that call's mutation is still held, traps: `begin` returns a
   `mutation` and has no error result to carry an `fs-error`, and waiting would park the call
   on the gate it holds and keep the shared `WriteGate` from every other agent until the epoch
   deadline. The trap ends the call, so the host drops the mutation the store holds and
   releases the gate at once. A component that drops its mutation first may `begin` again.
3. **Rechecks under the gate**, reading the target's current contents:
   - *call read record*: when this call read the path, the current digest must equal the
     digest it read; otherwise the file changed while the component computed outside the gate
     and the change is refused with the native stale message;
   - *observed mode* (edit, write): an existing target must be `unchanged` in the agent's
     `ObservedFiles`; `never-observed` and `changed-since-observed` are refused with the
     native tools' texts ("You must read … before changing it.", "… changed on disk since you
     last read it; read it again.");
   - *patch-authorized mode* (patch): the observation check is skipped (the ADR-0025
     exemption), the call read record check is not.
   A refusal is `fs-error.io` carrying the host's message, since the frozen `fs-error` has no
   staleness case; the message is the host's and safe to show the model.
4. **Stages** the new contents in a uniquely named sibling temporary file in the target's
   directory, synced, with the target's permission bits (today's `write_atomic`).
5. **Applies** atomically per file: the temporary file is renamed over the target
   (`write`), linked only if nothing is there (`create`, else `already-exists`), the target is
   unlinked (`remove`), or the source is moved only if nothing is at the destination
   (`rename`, else `already-exists`).
6. **Records** the result in the agent's `ObservedFiles` (the written contents; a removed or
   renamed-away path is forgotten), so consecutive edits need no re-read, exactly as the native
   tools record after writing. Patch-authorized writes are recorded too (ADR-0025: patch "still
   records what it wrote").

The gate is released when the guest drops the resource or, at the latest, when the export call
that began it returns: the host drops what the store still holds, and a later use traps
([`wit.md`](wit.md), "Streaming resources": a mutation is call-scoped). A trap or a deadline
mid-mutation releases the gate the same way and never undoes a change already applied.

### Reading outside the gate, and what ADR-0032 then guarantees

A component validates its input, reads and computes its change **before** `begin`, so the gate
is held only for the recheck and the write. The recheck of step 3 is what keeps that safe: a
second writer is checked against what the first one wrote, under the gate, and is refused
instead of overwriting, so no update is lost between file-tool mutations. For `edit` and
`write` the refusal is the native stale-file error they give today. For `patch` it narrows one
native behaviour: when another agent wrote a file between the patch's read and its mutation,
the native tool (which plans under the gate) would re-match its context lines against the new
contents, whereas the component's change is refused as stale and the model applies the patch
again. The frozen WIT also allows a component to `begin` before it reads, holding the gate
across its reads as the native tools do; a tool that needs the native re-match may do that,
at the cost of holding the gate while it computes.

### Directory-relative operations

Steps 3 to 5 do not reuse a path string resolved earlier. `commit` opens each ancestor
directory of the target relative to the workspace root's directory handle, one component at a
time, refusing to follow a symlink at any step, and performs the read, the staging, the rename
and the unlink relative to the final directory handle. A symlink swapped into the path after
resolution therefore cannot redirect the write outside the workspace or onto another file: the
operation acts on the directory it already holds. An in-root symlink the model named is
resolved first (step 1) and then walked as its canonical path. The crate that supplies the
directory-relative calls without `unsafe` is U-mut's choice; `rustix` is already in the
lockfile.

### What atomicity means

Atomicity is **per file**: a reader never sees a partial file, and a failed change leaves that
file as it was. A component that changes several files (a patch touching three files) makes
several changes under one held gate, so no other file tool interleaves, but a crash or a trap
between two changes leaves the earlier ones applied. There is no multi-file crash atomicity,
exactly as the native `apply_patch` has none today; a patch validates every hunk before its
first change, so an invalid patch changes nothing.

## Capability allocation per filesystem tool

What each assembly links, narrowed from the `tool` row of
[`modules/capabilities.toml`](../../../modules/capabilities.toml) by its manifest and its
catalog row (U-cat):

| Tool | `workspace` | `snapshot` | `workspace-mutation` | Mode |
|---|---|---|---|---|
| read | metadata and read, with observations | yes | — | — |
| search | metadata, list and read, without observations | — | — | — |
| edit | read | `check` and `observe` | yes | observed |
| write | read | `check` and `observe` | yes | observed |
| patch | read | — | yes | patch-authorized |

Edit and write link `snapshot` beside their observed mutation, one grant wider than the brief's
U-cat row reads ("workspace read plus observed mutation"): without `check` the native order of
refusals cannot be kept ("Error precedence" below), and ADR-0088 point 4 records the grant for
that reason. Each of the two catalog rows (U-cat) therefore links `snapshot` as well — a
component's import that its assembly does not link fails assembly, and both components import
it.

The patch exemption is a per-assembly grant in the host catalog (the mutation mode), not a WIT
capability and not a manifest field: the frozen interfaces and manifest fields stay as they
are, and a component cannot ask for the exemption, it can only be assembled with it.

### Error precedence (U-edit.1, U-write.1): the native order is kept

The native edit reports "You must read d.txt before changing it." before it matches
`old_string`, and the native write reads an existing target and checks it before it writes. A
component that computed first and learned the observation state only from its mutation would
report its own error first (a match failure on a file the model never read) and be refused
under the gate only afterwards. Edit and write therefore link `snapshot` — the grant the table
above records — and call `check` on the contents `workspace.read` returned, before they decide
anything. What the model sees is the native order of errors; what enforces it stays the host's
recheck of step 3 under the gate, so a component that skipped its own check would still be
refused.

## Open points for the slices

- **Linking** (BLOCKERS.md S2-B3): the fixture loader links only `control`, `clock`, `random`
  and `process`; the three workspace interfaces are refused until the loader links them.
- **Package names** (BLOCKERS.md S2-B2): packages live at `modules/p1-module-{edit,write,patch,search}/`.
