# Capabilities and the unsafe policy: the boundary check

Status: published freeze items 11 and 13 of the WebAssembly boundary (ADR-0071). The frozen
data is [`modules/capabilities.toml`](../../../modules/capabilities.toml), the check is
[`scripts/check-module-boundaries.sh`](../../../scripts/check-module-boundaries.sh), the
allocation table it comes from is in [`wit.md`](wit.md), and the build it reads is
[`package.md`](package.md).

## The allocation as frozen data (freeze item 13)

`modules/capabilities.toml` holds one table per module class — `tool`, `provider`,
`context-policy`, `authorization-policy`, `workflow-implementation` and `workflow-decision` —
listing the `p1:module` interfaces a module of that class may import. It is the "Per-class
capability allocation" table of [`wit.md`](wit.md) as data, with two additions the table leaves
out because they grant nothing: `types` in every class, and `worker-types` in the classes with
the worker interfaces. Both carry types only, so a component may import them without declaring
them.

The file is frozen with `wasm-boundary-v1`. A change to it is a boundary change: it narrows or
widens what every module of a class may reach, so it takes the boundary's own review, not an
ordinary edit. `scripts/build-modules.sh` reads it to refuse a package whose manifest declares
a capability outside its class, and `scripts/check-module-boundaries.sh` reads it to check a
built component.

A manifest narrows the allocation, never widens it: `capabilities` in
`[package.metadata.p1-module]` lists a subset of the class's row. A world imports the union of
the class's row, so the interfaces exist in the WIT; the host links only what the manifest
grants, and the capability check compares a built component's actual imports to that narrowed
set.

## What the boundary check compares (freeze item 13)

`scripts/check-module-boundaries.sh` builds nothing. It reads the outputs
`scripts/build-modules.sh` wrote under `modules/target/p1-modules/` (or the directory
`--output-dir` names) and the frozen data, and for every module package checks:

- the manifest's `kind` is a class of `modules/capabilities.toml`;
- every manifest capability is in that class's allocation;
- the built manifest's `kind` and `world` are the package manifest's, and the world is the
  world of its kind (`p1:module/<kind>@1.0.0`);
- the component's exports are the manifest world's exports, so the component's world is the
  manifest's world;
- the `<package>.imports` list is the component's own import list, cross-checked against a
  fresh `wasm-tools component wit` extraction, so an edited or stale list is a finding;
- every import is a `p1:module` interface of the class allocation and either a type-only
  interface or a capability the manifest declares; any other import is a finding;
- every `wasi:` import is a finding, no exceptions.

The build writes `<package>.imports` from the extracted world and already refuses a component
with a `wasi:` import, but the check does not trust that file: it re-extracts the world and
compares, so the check finds a hand-edited list. `wasm-tools` is the only tool it runs.

When a package's outputs are missing the check names `scripts/build-modules.sh` and exits 1:
the build is a separate step, and the boundary check never builds.

### Why `wasi:` is always refused (D-XO-4)

A guest targets `wasm32-unknown-unknown`, componentized with no WASI adapter, so a built
component has no `wasi:` import to begin with. A `wasi:` import means std I/O or a WASI adapter
got in. WASI is not part of the approved dependency set — the host does not link
`wasmtime-wasi` — so such a component would not run, and a component that could read the
environment, the filesystem or the clock through WASI would hold a capability outside the
allocation. The owner's decision D-XO-4 is therefore the rule the check enforces: the boundary
check rejects every `wasi:` import, no exceptions. A capability a module needs is a `p1:module`
interface, granted through its manifest and linked by the host.

## The unsafe policy (freeze item 11)

Every crate of the module workspace, every shared guest crate a package depends on by path from
outside `modules/`, and the host crates `p1-module-runtime`, `p1-module-protocol` and
`p1-module-tests` carries `unsafe_code = "forbid"`. In each crate that is a `Cargo.toml` with
`[lints] workspace = true` and a workspace root with `[workspace.lints.rust] unsafe_code =
"forbid"`; the check reads both tables and reports a crate that does not inherit the
prohibition, or a workspace that does not set it.

The check scans each crate's handwritten source and reports a crate whose code names the
`unsafe` keyword. It removes comments and string and character literals before the scan, so a
doc comment that discusses `unsafe` is not a use of it, and it reads `'a` as a lifetime rather
than a literal.

Generated code is reported, not failed. The generated bindings of a world live in their own
crate (`modules/p1-bindings-tool`, `modules/p1-bindings-workflow-decision`): `generate!` runs
in the bindings crate and `pub_export_macro` exports the `export!` macro a component crate
calls. The reason is the lint: the `export!` macro `wit-bindgen` writes needs `unsafe`, and a
macro defined and expanded in the same crate fails the prohibition there, so the bindings are
defined in one crate and expanded in another. The check names every crate whose code is
generated by `wit_bindgen::generate!` or `wasmtime::component::bindgen!`, says that the
expansion contains `unsafe` code the lint does not see for that reason, and says that the crate
itself compiles under `unsafe_code = "forbid"`. A crate that loosens the lint, or a handwritten
crate that uses `unsafe`, is a finding.

## Checks

- `scripts/check-module-boundaries.sh` prints one line per package and crate, and ends
  `check-module-boundaries: clean (<n> packages, <m> crates)` when there is no finding, else
  `check-module-boundaries: <k> finding(s)` and exit 1.
- `scripts/gate.sh` runs it in the modules hook, after `scripts/build-modules.sh --all`.
- A copy of a package's outputs with an added `wasi:` import line is a finding; the check
  documents `--output-dir` for that.
