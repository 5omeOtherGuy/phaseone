# Module packages: format and identity

Status: published freeze item 6 of the WebAssembly boundary (ADR-0071). The build is
[`scripts/build-modules.sh`](../../../scripts/build-modules.sh), the manifest checks it makes are
here, the workspace is [`modules/Cargo.toml`](../../../modules/Cargo.toml) and the first package is
`modules/p1-module-fixture/`. The loader's own rules (verify the digest, then compile those same
bytes, never a compiled cache, official source only) are the runtime crate's and are published
with it.

## The package

A module package is a directory `modules/p1-module-*/`: an ordinary crate of the module workspace
whose Cargo.toml carries a `[package.metadata.p1-module]` table. Nothing else is a package — the
workspace root, `modules/wit/` (the WIT worlds) and a bindings crate (`modules/p1-bindings-*`)
are not — so adding a package is adding a directory, and the build finds it by that name.

The module workspace is separate from the host workspace: its crates link `wit-bindgen` and build
for the wasm target only, and the host crates never depend on them.

## Manifest fields (frozen)

| Field | Meaning |
|---|---|
| `name` | the package's identity, `<namespace>/<name>` |
| `kind` | the module class: `tool`, `provider`, `context-policy`, `authorization-policy`, `workflow-implementation` or `workflow-decision` |
| `world` | the WIT world the package implements: `p1:module/<kind>@1.0.0`, the class's world in the package of [`wit.md`](wit.md) |
| `protocol` | the major.minor of the value protocol the module speaks: `p1-module-protocol`'s `PROTOCOL_VERSION` ([`protocol.md`](protocol.md)) |
| `capabilities` | what the module may be linked with, a subset of its class's allocation in [`wit.md`](wit.md) |
| `variant` | the model-facing variant of the loader-built `ToolIdentity`: two packages may ship the same tool under different variants |

Every field is present in every package. The build refuses a package with an explicit message when
a field is missing, `kind` is not one of the six, `world` is not the world of its kind, `name` is
not `<namespace>/<name>`, or a capability is outside the class's allocation (the frozen data in
[`modules/capabilities.toml`](../../../modules/capabilities.toml), the allocation table of
[`wit.md`](wit.md)).

### The reserved `p1/` namespace

`name`'s namespace is `p1`: the packages p1 builds and ships. A package from anywhere else is not
an official package, and the loader refuses what p1 does not build (freeze item 6, "official
source only"). The build refuses a package whose namespace is not `p1` here, so no unofficial
package is ever published from this repository.

## Identity: the digest

A module's identity is the digest of the built `.wasm`: the same name with different bytes is a
different module, and the same bytes under a different path is the same module. The build writes
the `sha256sum` line of the component to `<package>.sha256` and the same value as
`digest` (`sha256:<hex>`) in the manifest, beside `size` in bytes. The loader verifies the bytes
against the digest before it compiles them; manifest name, digest and the release manifest it came
from are what a call's provenance rests on.

## The loader-built `ToolIdentity`

A tool's identity is built by the loader, never reported by the module about itself: the `tool`
world has no export that returns one, so a module cannot claim another implementation's identity
or its grants. The implementation part comes from the manifest `name` and the variant part from
the manifest `variant`; the model-facing name of a call is the interface's own business
(`declaration`), and an environment may present the tool under another name.

## Build outputs

`scripts/build-modules.sh --package <package>` (or `--all`, the default) writes into
`modules/target/p1-modules/<package>/`:

| File | Contents |
|---|---|
| `<package>.wasm` | the component: the shipped artifact, the digest's input |
| `<package>.wit` | its world, extracted with `wasm-tools component wit` |
| `<package>.sha256` | the `sha256sum` line of `<package>.wasm` |
| `<package>.imports` | every imported interface, one per line, sorted (this is what the capability check compares against) |
| `<package>.manifest.json` | the manifest fields above, plus `digest` and `size` |

The build compiles the package with the target named by `WASM_TARGET` in
[`modules/toolchain.pins`](../../../modules/toolchain.pins) — the pin alone decides, so a target
that produces a core module rather than a component is componentized with
`wasm-tools component new` — then validates the component with `wasm-tools validate` and prints one
line per package, `build-modules: <package> ok sha256:<digest> (<size> bytes)`. Nothing but the
component is shipped; the other four files are what the host and the checks read.

## The binding-crate pattern

Generated bindings live in `modules/p1-bindings-tool`: `wit_bindgen::generate!` runs inside a
`pub mod generated` with `pub_export_macro: true` and `default_bindings_module:
"p1_bindings_tool::generated"`, and a component crate implements the world's `Guest` trait and
calls `p1_bindings_tool::generated::export!(Type)`. The reason is the lint: every crate of the
module workspace forbids `unsafe_code` (the workspace lint), and the generated `export!` macro
needs `unsafe`, so it must be defined in one crate and expanded in another; a macro defined and
expanded in the same crate fails the lint there. The pattern also keeps all generated code in one
crate, where a boundary check can tell generated from handwritten code.

## The release profile

Modules are built and shipped in release (`[profile.release]` in `modules/Cargo.toml`):
`panic = "abort"`, `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `strip = true`. A shipped
module is a small component, and `panic = "abort"` is part of the boundary: a guest panic is the
wasm `unreachable` trap, which the host maps through `ModuleFailure` like any other trap, never an
unwinding guest.

## The guest target (S0-Q9)

The guest target is `wasm32-unknown-unknown`, componentized with `wasm-tools component new` and no
WASI adapter (decision D-XO-4 on S0-Q9): a guest has no std I/O by design, so a built component
imports only `p1:module` interfaces. The build refuses a package whose `<package>.imports` lists
any `wasi:` interface, and the loader refuses such a component too, no exceptions.
