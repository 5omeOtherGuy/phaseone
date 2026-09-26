---
adr: {{number}}
title: Module identity and verified loading
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0016, ADR-0021, ADR-0065, ADR-0071, ADR-0079, ADR-0081, ADR-0082, migration plan findings F7 and F8, freeze items 6, 12 and 13]
---
# ADR-{{number_padded}}: Module identity and verified loading

## Context

ADR-0071 has the host load tools, providers and policies as WebAssembly packages by the
names an environment file assembles, and keeps the assembly rule: a module the
environment does not name is never instantiated and cannot dispatch. Until now a tool's
`ToolIdentity` was built from the compiled-in crate, and the host recognised the shell
tool by a literal implementation name (`SHELL_IMPLEMENTATION`). With packages, a name no
longer pins the bytes, and a host check against a literal name would trust whatever
package claims it. F7: packages come from p1's official release only. F8: lock
ownership: shipped resolutions travel in the release archive, and overrides follow the
existing environment override order. ADR-0079 defines the release manifest
(`modules/manifest.json`, format `p1-release-manifest/1`) that binds a release's package
files to their sha256.

## Decision

- **Identity is the digest.** A package is a manifest plus a component
  (docs/design/modules/package.md); its identity is `sha256:<hex>` of the component's
  `.wasm` bytes. The same name with other bytes is another module.
- **Environments name modules; `modules.lock` resolves them.** An environment keeps
  naming a module by its `[[tools]] module = "<name>"` key; no new environment field is
  needed. A key no compiled-in tool claims is resolved through `modules.lock`
  (TOML, `format = "p1-modules-lock/1"`), one table per module name:
  `[modules.<name>]` with `package` (the manifest `name`, reserved `p1/` namespace),
  `version` (the release version, recorded), `digest` (`sha256:<64 hex>`), `world` and
  `protocol` (the ABI). Unknown keys are refused, so a lock cannot name a path. Parsing
  and resolution are plain data in `p1-assembly` (`ModulesLock`, `load_modules_lock`),
  with no wasmtime there.
- **Override order.** A lock lives next to each environments directory
  (`<dir>/../modules.lock`, as profiles do) and the locks layer in the environment search
  order: a higher-priority directory's entry replaces the entry of the same name. The
  repository ships `modules.lock` at its root; in a release it travels in the archive
  (the manifest's `environment_locks`). An override can only select: the loader accepts
  nothing but official packages, whatever a lock says.
- **Official source only.** p1's release module set is
  `<exe dir>/../share/p1/modules/manifest.json` (ADR-0079), never a configuration
  directory. The one loader is S0's `p1_module_runtime::Loader` (docs/design/modules/
  package.md, "The loader"): it loads by manifest name only, refuses a name outside
  `p1/` (`NotOfficial`) or one the release manifest's `components` do not list
  (`NotInManifest`). The host wraps every loader refusal with the module name and the
  lock file that selected it, so the error names the source of the selection; a lock
  entry outside `p1/` or with any key but the five above is refused while the lock is
  read, naming the lock file.
- **Verify, then compile the same bytes** is the runtime loader's: the component is read
  once, hashed, compared with the release manifest's digest (`DigestMismatch`) and those
  bytes compiled; no compiled cache is ever deserialized. The host keeps no second
  verifier.
- **One identity, one component.** The release manifest is refused whole when two
  components claim one name or one digest (`ManifestError::DuplicateIdentity`, added to
  `p1-module-runtime`'s `manifest.rs`: a duplicate name when the manifest is parsed, a
  duplicate digest by `ReleaseManifest::check_unique_digests`, which the host runs on the
  release it loads from), so neither a lock resolution nor a journal's
  recorded module identity can be ambiguous.
- **The lock pins what the release ships.** Before the loader runs, the host compares
  the lock entry's `digest`, `world` and `protocol` with the release manifest's entry
  (`ModulesError::LockMismatch`), so an override lock cannot silently select other bytes
  or another ABI than it states. The ABI the runtime accepts is its own: the world of the
  kind (`ModuleKind::world`, `WorldMismatch`) and the protocol major of
  `PROTOCOL_VERSION` (`ProtocolMismatch`); a newer minor is the value protocol's business
  (protocol.md).
- **Grants.** A granted capability outside the class allocation of
  `modules/capabilities.toml` (freeze item 13, compiled into the host) is refused
  (`CapabilityNotAllocated`); the runtime refuses a grant it cannot link
  (`UnsupportedCapability`) and a component import the manifest does not grant, every
  `wasi:` import included (`UndeclaredImport`). The three worker interfaces are three
  grants, never one.
- **The loader builds `ToolIdentity`** from the release manifest entry: implementation
  from `name`, variant from `variant`; a package does not name its own identity. Host
  behaviour that depended on which tool ran keys on semantic capabilities the manifest
  declares and the loader verifies (for example `records-command-evidence` replaces the
  literal `SHELL_IMPLEMENTATION` match; a later slice).
- **Registration and the assembly rule.** The catalog build path
  (`catalog/mod.rs` → `catalog/modules.rs`) loads what the effective lock resolves and
  registers each verified tool package under its module name, after the compiled-in
  tools; a name a compiled-in tool already has is refused rather than replaced, and a
  non-tool package is refused until its class has an adapter. An empty lock loads
  nothing and needs no release. Registration instantiates nothing: the catalog factory,
  which builds the generic tool adapter (`wasm_tool`, freeze item 12) with the
  capability services of the assembling agent, runs only when an environment assembles
  the name. An installed but unselected package and an invented name therefore both
  fail to dispatch. A name, description or variant override on a module is refused, as
  `WasmTool` has no face to apply.
- `p1 modules list/inspect/verify` shows availability, selected versions, digests,
  imports and effective grants; `verify` is metadata-only and reads neither credentials
  nor user configuration (a later S1 slice).

## Consequences

- A tampered or foreign package cannot run under an official name, and the journal's
  assembly identity (see "Execution manifests in journals") names the exact bytes.
- Every host start with a non-empty lock verifies and compiles each locked package
  once, before any run; a corrupt release stops the host rather than a single tool.
- Third-party modules need a later ADR that relaxes the official-source rule.
- The lock entry's `version` is informational: the package manifest carries no
  version, so the digest is what pins the bytes.

## Alternatives considered

- Trusting names and versions without digests: rejected (F7).
- A path or URL field in the lock: rejected; the lock selects, the release supplies.
- Signed packages from any source: deferred; it needs key management p1 does not have.
- A compiled-module cache: rejected; deserializing compiled code is the unsafe path the
  freeze excludes.
- Loading lazily on first assembly: not chosen for the first cut; eager verification
  reports a broken release at start-up, and compiling does not instantiate.

## Evidence

`cargo test --locked -p p1-module-tests --test loader` (digest mismatch with the
corruption fixture, unsupported world and protocol, duplicate name and duplicate digest
with the collision fixture, non-official source, lock-vs-release mismatches, ungranted
import, grant outside the allocation, one distinct error per refusal);
`cargo test --locked -p p1-module-tests --test assembly` (installed-but-unselected,
unresolved and invented-name cases, with a selected-package control built by the real
`WasmTool`); `cargo test --locked -p p1-assembly --test modules_lock` (lock format and
override order). PRs and merge commits are cited when this ADR is flipped to accepted.
