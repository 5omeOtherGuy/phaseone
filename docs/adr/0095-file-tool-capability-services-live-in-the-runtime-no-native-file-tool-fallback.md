---
adr: 95
title: File-tool capability services live in the runtime; no native file-tool fallback
status: accepted
date: 2026-09-27
deciders: lead
supersedes: []
superseded_by: []
sources: ["docs/design/modules/capabilities.md", "docs/design/modules/workspace-mutation.md"]
---
# ADR-0095: File-tool capability services live in the runtime; no native file-tool fallback

## Context

S2 activation (PR #386, ADR-0092) made `modules.lock` entries named `edit`, `write`,
`apply_patch` and `grep` resolve to the `p1/edit`, `p1/write`, `p1/patch` and `p1/search`
components, exactly as S1.8 did for `read`, and kept the native tools registered as the
fallback for a key no lock selects. The host also needed HOST-side code to link those
components: `p1_tool_read::ReadCapability` (the confined `workspace` and `snapshot`
services, the call-scoped read record and the credential refusal of issue #142),
`p1_tool_write::MutationCapability` (`workspace-mutation` over the agent's write gate) and
`p1_tool_search::SearchCapability` plus its walk. Because a host service must be linked
from the host's own assembly, `cargo tree --locked -p p1-host -e normal` reached the five
tool crates and their four `p1-tool-*-logic` crates: twelve native fallbacks in the
cutover audit, where the owner's rule is ZERO (`cutover-fallbacks/PLAN.md` row
"p1-tool-read · S1").

The S7.10 slice R1 removes them: read, edit, write, apply_patch and grep are served by
their components alone, and the host no longer depends on a tool crate.

## Decision

The file-tool capability services are the HOST's, and no native file tool is registered.

1. `p1-module-runtime` gains `file_services.rs` (the read side, the walk and the owned
   mutation a component is linked with, and the builders the catalog uses) and
   `file_walk.rs` (the `.gitignore`-aware walk behind `workspace.list-files` and
   `workspace.search`, over the ripgrep crates `ignore` and `grep`). The adapter sits where
   the capability traits and `p1-workspace` meet, as the runtime's other per-contract
   adapters do.
2. `p1-workspace` gains `policy.rs`: the credential refusal that comes BEFORE confinement
   (`refuse_credentials`, `refuses_credentials`, the XDG-named stores) and the two
   model-facing texts it and a failed read carry. The native read tool and the capability
   service both run that one copy; `p1-read-guest` keeps the guest-side copy the component
   words its own failures with, and `p1-tool-read` pins the two copies equal.
3. The tool crates re-use the moved code: `p1-tool-read` and `p1-tool-write` re-export the
   services the host links, and `p1-tool-search`'s native `grep` runs the host's walk
   through the runtime's value types, so native and component walk the same files with the
   same wording.
4. `crates/p1-host/src/catalog/tools.rs` registers no file tool: `read`, `edit`, `write`,
   `apply_patch` and `grep` are `HOST_ENTRIES` beside `shell` and `finish`, so each key is
   loaded from the release manifest, verified against it and linked through the host's own
   service hook. A release that does not carry one of them fails the catalog build naming
   the module — there is no compiled-in tool to fall back to. A `modules.lock` entry that
   names one of the keys still wins over the release's, exactly as it does for `read`.
5. `crates/p1-host/Cargo.toml` keeps the five crates only as `[dev-dependencies]`: the
   host's own cases build the native tools to compare a component against them.

## Consequences

* `cargo tree --locked -p p1-host -e normal` (default and `--all-features`) reaches none of
  `p1-tool-read`, `p1-tool-edit`, `p1-tool-write`, `p1-tool-search`, `p1-tool-patch` and no
  `p1-tool-*-logic` crate, which is the cutover's condition for the five keys.
* A host service now lives with the capability traits rather than with the tool it serves:
  the runtime crate carries the walk and therefore depends on `p1-workspace`, `ignore` and
  `grep`. That is the price of a host that compiles no tool crate; the per-contract adapter
  pattern is unchanged.
* The credential refusal policy has two copies of its two texts (the host's in
  `p1-workspace`, the component's in `p1-read-guest`, which cannot depend on a crate that
  uses `rustix`). `the_moved_texts_are_the_guests` in `crates/p1-tool-read/src/lib.rs` fails
  if they ever drift.
* The five keys are host entries, so an installed release must ship their packages: a
  release (or a lock) that cannot serve one fails the assembly naming the module instead of
  quietly dispatching a native tool.

## Alternatives considered

* **Keep the services in the tool crates and move only the credential policy.** The host
  would still depend on the five crates (their services are linked per assembly), so the
  fallbacks would stay and the audit's zero-fallback condition would not hold.
* **Put the capability adapters in `p1-host` (`catalog/file_services.rs`).** The tool
  crates could then not re-use them (a cycle), so each native tool would need a second copy
  of the walk and the read side, and the crate that becomes a component would no longer run
  the code its component runs.
* **Put the walk and the read side in `p1-workspace`.** That foundation crate would gain
  the runtime's traits and `wasmtime`'s async futures; it is deliberately free of both.
* **Leave the four keys resolvable only through `modules.lock`.** The shipped lock is empty
  (D083b), so an environment naming `edit`, `write`, `apply_patch` or `grep` would fail to
  assemble in a shipped install.

## Evidence

* `flock /tmp/cutover-cargo.lock cargo tree --locked -p p1-host -e normal` and the same with
  `--all-features`: no line names one of the five crates or a `p1-tool-*-logic` crate (PR
  body carries both greps).
* `cargo test --locked -p p1-workspace -p p1-module-runtime -p p1-tool-read -p p1-tool-write
  -p p1-tool-search -p p1-tool-edit -p p1-tool-patch`: the moved services' own cases
  (`crates/p1-module-runtime/src/file_services.rs`), the credential policy's
  (`crates/p1-workspace/src/policy.rs`) and each tool's native suite.
* `crates/p1-module-tests/tests/filesystem_tools.rs`, `filesystem_boundary.rs`,
  `read_module.rs`, `installed_release.rs`: the components over the host's services, and the
  native/component parity cases (CI builds the packages; the local run used the published
  release's packages).
* `crates/p1-host/src/catalog/tools.rs` (`the_file_tool_keys_have_no_native_registration`)
  and `crates/p1-host/src/catalog/modules.rs`
  (`the_file_tool_keys_are_release_host_entries`): the keys carry no native registration and
  a release missing one is refused by name.
