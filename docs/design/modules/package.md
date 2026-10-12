# Module packages: format and identity

Status: published freeze item 6 of the WebAssembly boundary (ADR-0071). The build is
[`scripts/build-modules.sh`](../../../scripts/build-modules.sh), the manifest checks it makes are
here, the workspace is [`modules/Cargo.toml`](../../../modules/Cargo.toml) and the first package is
`modules/p1-module-fixture/`. The loader's own rules (verify the digest, then compile those same
bytes, never a compiled cache, nothing beyond the class allocation, installed sets only) are the
runtime crate's and are published below under "The loader"; the decisions behind them are
[ADR-0082](../../adr/0082-component-abi-and-execution-ownership.md) and
[ADR-0209](../../adr/0209-external-modules-any-publisher-s-package-loads-by-its-lock-entry-verified-by-digest.md),
which lets a package of any publisher load once a user installs it.

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
| `kind` | the module class: `tool`, `provider`, `context-policy`, `authorization-policy`, `workflow-implementation`, `workflow-decision`, `credential-method` or `usage-probe` |
| `world` | the WIT world the package implements: `p1:module/<kind>@<version>`, the class's world in the package of [`wit.md`](wit.md) at the WIT version it was built with (this repository's packages: `1.1.0`) |
| `protocol` | the major.minor of the value protocol the module speaks: `p1-module-protocol`'s `PROTOCOL_VERSION` ([`protocol.md`](protocol.md)) |
| `capabilities` | what the module may be linked with, a subset of its class's allocation in [`wit.md`](wit.md) |
| `variant` | the model-facing variant of the loader-built `ToolIdentity`: two packages may ship the same tool under different variants |
| `token_origins` | a `credential-method` or `usage-probe` package only: the https origins its refresh requests go to ([`credential.md`](credential.md), ADR-0212), or its usage request goes to ([`usage-probe.md`](usage-probe.md), ADR-0214) |

Every field but `token_origins` is present in every package. The build refuses a package with an
explicit message when a field is missing, `kind` is not one of the seven, `world` is not the world of its kind, `name` is
not `<namespace>/<name>`, or a capability is outside the class's allocation (the frozen data in
[`modules/capabilities.toml`](../../../modules/capabilities.toml), the allocation table of
[`wit.md`](wit.md)).

### Namespaces and the reserved `p1/` (ADR-0209)

`name`'s namespace names the publisher. `p1` is reserved for the packages this repository builds
and ships in p1's release: only the release's own set may hold a `p1/` package, `p1 modules
install` refuses one, and the build refuses a package here whose namespace is not `p1`. Any other
namespace is chosen by its publisher and owned by nobody: it is a label for people, not a trust
claim. Two packages with one name and different bytes are two modules; the user's lock decides
which one runs by its digest, and no registry is consulted.

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

## Installed sets (ADR-0209)

A module comes from an installed set: a directory holding a `manifest.json` in the release
manifest format of [`manifest.rs`](../../../crates/p1-module-runtime/src/manifest.rs) (format
`p1-release-manifest/1`, ADR-0079), whose `components` list names each package by its manifest
`name`, pins its bytes by `digest`, locates them by a path relative to the manifest and carries
the frozen manifest fields above. Three kinds of set exist, searched in the environment search
order of ADR-0201:

| Set | Where | Written by |
|---|---|---|
| the user's | `<config dir>/modules/` (`$P1_CONFIG_DIR`, else `~/.config/p1`) | `p1 modules install` |
| a pack's | `<pack>/modules/` | the pack's author |
| the release's | `<exe dir>/../share/p1/modules/` | p1's release |

An environment names a module by its lock key, and the effective `modules.lock` (the user's, then
each pack's, then the shipped one, ADR-0087) pins the key to a package name, a digest, a world and
a protocol. The host (`crates/p1-host/src/catalog/modules.rs`) takes the first set that holds the
package at the lock's digest; a `p1/` package comes from the release's set alone. It refuses a
package no set holds (`LoadError::NotInstalled`, naming the lock and the sets searched), a pin no
set satisfies (`LockMismatch`, naming the lock, the key and the set it compared), and a set other
than the release that lists a `p1/` package or whose directory another user could write (the
pinned-directory rule of ADR-0108) whole. The shipped lock at the repository root names only `p1/`
packages (`ModulesLock::parse_shipped`), so a release stays what p1 built.

## Package archives and `p1 modules install` (ADR-0209)

A package travels as one file, `<package>.p1pkg`: an uncompressed ustar archive of exactly two
regular files, `manifest.json` and `component.wasm`. The manifest is the build's
`<package>.manifest.json` (above) plus `format` (`p1-module-package/1`) and `version`, the version
the package is published at; unknown fields are refused. `scripts/pack-module.py <build dir>
--version <v> --output <file>` writes one from a package's build outputs, deterministically.

`p1 modules install <file>` is the one explicit install path. It refuses an archive with any other
member, a `p1/` name, or a component whose digest or size is not the manifest's; loads the package
through the runtime loader exactly as a session would, so nothing the loader refuses is recorded;
then copies the component into the user's set (content-addressed, `packages/<sha256 hex>.wasm`),
lists it in the set's manifest and writes the user's `modules.lock` entry for the module key its
name declares (the name without its namespace), printing the digest it pinned and the package it
replaced. Nothing is fetched and nothing else installs a package: not a model, an environment, a
route or a network request inside p1. `p1 modules list`, `inspect` and `verify` cover every set
and name the set of each package.

## The loader (freeze item 6)

The loader is [`crates/p1-module-runtime/src/loader.rs`](../../../crates/p1-module-runtime/src/loader.rs),
one `Loader` per installed set.

- **Installed sets only.** `Loader::load` takes a name, never a path or bytes. A name the set's
  manifest does not list is `LoadError::NotInstalled`; each error says why, naming the module.
- **Refused before anything is read.** A `kind` the runtime does not speak (`UnknownKind`), a
  `world` that is not the world of its kind at a WIT version the runtime serves
  (`WorldMismatch`), a `protocol` the runtime does not serve (`ProtocolMismatch`; both by the
  version rule of ADR-0210: the runtime's major, any minor up to its own), a grant outside the
  class allocation (`GrantExceedsAllocation`) and a granted capability the runtime cannot link
  (`UnsupportedCapability`) are refused from the manifest entry alone. The runtime links
  `control`, `clock`, `random` and `process` today; the other capabilities arrive with the
  streams that own their native services, and until then a grant of one is refused.
- **Installer metadata check.** `p1 modules verify` uses the loader's shared
  manifest-field and component-header checks (magic, component version 13 and layer 1),
  without compiling or opening user configuration. The loader refuses the first metadata
  error; installer verification reports all independent world and protocol errors for an
  entry. `--integrity-only` still reports but tolerates grants that the current runtime
  cannot link.
- **Verify, then compile the same bytes.** Every path component below the module root is
  opened relative to its directory handle with `O_NOFOLLOW`; parent and final symlinks are
  refused. The final handle must be a regular file (`fstat`); its bytes are read once,
  hashed with SHA-256 and compared with the manifest
  digest (`DigestMismatch` names both and the set), and only then are *those* bytes compiled from memory with
  `Component::from_binary`. Nothing is read twice, so a file swapped between the check and the
  compile cannot be the one compiled, and no text format is accepted.
- **Only the release's own compiled copy is deserialized.** wasmtime is built without its
  `cache` feature ([`toolchain.md`](toolchain.md#the-pins)). A release ships each component's
  ahead-of-time compiled copy `<package>.cwasm`, and the manifest entry pins it under
  `precompiled` with the digest of its bytes (ADR-0113). Only the running binary's installed
  `<exe parent>/../share/p1/modules` root authorizes these copies: the host derives and passes
  it explicitly to `Loader::for_installation`. Other roots and `Loader::new` ignore compiled
  entries and compile the verified component. After the component verified at that trusted
  root, the loader opens the copy by the same no-symlinks rule, reads it once, compares its
  digest, and only then deserializes *those* bytes
  from memory (`Component::deserialize`, never `deserialize_file`); a digest mismatch refuses
  the load, and a copy wasmtime refuses (another wasmtime, another engine configuration) is
  answered by compiling the verified component. Trust requires the installation root as
  well as the digest checks; `modules inspect` reports the construction path actually used.
- **Compiled once per process.** Every loader of a process runs on one engine, which keeps the
  components it built in memory, keyed by verified digest and installation trust domain
  (ADR-0112, ADR-0113). Untrusted loads cannot reuse a deserialized component. A load
  whose bytes verify to a digest already compiled takes that component; the read, the digest
  check and the import check below still run on every load, and the name, class, grants and
  identity are the load's own manifest entry's.
- **Imports are checked against the class allocation, then the grant.** Every import of the
  compiled component must be an interface of its class's frozen allocation
  ([`modules/capabilities.toml`](../../../modules/capabilities.toml)) at a WIT version the
  runtime serves, whatever its manifest grants; anything else, every `wasi:` import included, is `LoadError::ImportsExceedAllocation`
  (ADR-0209: the build-time comparison of `scripts/check-module-boundaries.sh`, made at load time
  for every set). Within the allocation, an import must be a type-only interface or a capability
  the manifest grants; anything else is `LoadError::UndeclaredImport`.
- **Identity.** The `LoadedModule` carries the verified digest, the class, the granted
  capabilities and the loader-built `ToolIdentity` (above).

The cases are tested in
[`runtime_spike.rs`](../../../crates/p1-module-tests/tests/runtime_spike.rs)
(`load_verifies_then_compiles_the_same_bytes`, `load_refuses_what_the_release_does_not_ship`), and
the installed sets, the archive and the allocation check in
[`external_modules.rs`](../../../crates/p1-module-tests/tests/external_modules.rs).

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

The release installer records `.p1-modules-required` independently of `modules/`, so
missing module directories cannot pass a matching-release no-op. A missing release
marker forces reinstallation instead of trusting the binary version alone.

The release installer rechecks the protected config path and the identities of `bin/` and
`share/` before pathname operations; an advisory lock on the prefix directory inode
serializes cooperating installers without creating a redirectable lock file.
Rollback refuses to delete a public destination whose identity no longer matches the
transaction's installed entry. These checks detect many concurrent mutations but are
not directory-handle-anchored: an uncooperative same-user actor can still race a check
and a write.

Each package is built in a sibling scratch directory and published only after all five
outputs validate. A failed rebuild retains the last complete package. A partial `--package`
build removes the whole-set development manifest: only `--all` can attest the complete
set as originating from the current commit. `--all` invalidates its prior whole-set
manifest before publishing the first package, so a failed batch cannot retain a stale
attestation beside partially updated packages.

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

## Shared guest logic (S0-R3)

Decision S0-R3, asked by S3 and decided by the S0 lead: guest logic MAY be a target-independent
library crate under `crates/` used by both the native adapter's tests (in `crates/p1-tool-<x>/`)
and the component package (`modules/p1-module-<x>/`), so the frozen native tests — for example
[`crates/p1-tool-shell/tests/output_filters.rs`](../../../crates/p1-tool-shell/tests/output_filters.rs),
byte-for-byte — keep running over the same code the component ships. The decision adds a crate
beside a package; it replaces no package and no world.

Rules:

- The crate lives in the root workspace under `crates/`, is a member of it, and never lives under
  `modules/`. The stream that owns the tool owns the crate; the module package depends on it by
  `path`.
- It is pure computation per D-XO-8: no filesystem, network, process, thread, clock or
  environment access, and no host-specific I/O. A guest that needs any of those goes through the
  imported capability interfaces of [`wit.md`](wit.md), never this crate.
- It depends only on std, `serde` (derive), `serde_json` and `regex`, at the versions the root
  `Cargo.lock` pins. It does not depend on `p1-contracts`, `p1-module-protocol` or any other
  host crate.
- It inherits the root workspace's `unsafe_code = "forbid"` (`Cargo.toml`).
- It compiles natively and for the guest target `wasm32-unknown-unknown`; the component build is
  what proves it, since a crate built only for the host workspace would not.
- `scripts/check-module-boundaries.sh` (slice S0.7) counts it as handwritten guest code.
