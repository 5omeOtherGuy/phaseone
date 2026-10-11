---
adr: 209
title: External modules: any publisher's package loads by its lock entry, verified by digest
status: accepted
date: 2026-10-11
deciders: owner
supersedes: [87]
superseded_by: []
sources: [ADR-0071, ADR-0079, ADR-0087, ADR-0200, ADR-0201, issue #886, issue #887]
---
# ADR-0209: External modules: any publisher's package loads by its lock entry, verified by digest

## Context

Owner, 2026-10-11, deciding wave 3 of the repository split (#886): "I want modules developed
outside this repository. The whole idea is that everybody can develop modules."

ADR-0087 made a module's identity the sha256 digest of its component bytes, had
`modules.lock` resolve the names an environment uses, and restricted the loader to p1's own
release: a package name outside the reserved `p1/` namespace is `NotOfficial`, a name the
release manifest does not list is `NotInManifest`, and a lock entry can only ever select an
official package. That rule was right while every module was built here; it is exactly what
keeps a module built anywhere else from loading, so it has to change. What must not change:
a name never pins bytes (the digest does), a module is never loaded from a path or from bytes
a caller hands in, the bytes are hashed and compared before they are compiled, no compiled
cache is deserialized, and a module gets no capability beyond its class's frozen allocation.

## Decision

- **Identity stays the digest.** A package is still a manifest plus a component, its identity
  `sha256:<hex>` of the `.wasm` bytes, whatever its source. Two packages with one name and
  different bytes are two modules.
- **Namespaces are publisher names.** `p1/` stays reserved for the packages this repository
  builds and ships in its release. Any other namespace is a publisher's name, chosen by the
  publisher and owned by nobody: it is a label for people, not a trust claim. Conflicts are
  resolved by the digest and by the user's lock, never by a registry.
- **Packages come from installed sets, not from paths.** An installed set is a directory
  holding a `manifest.json` in the release-manifest format (ADR-0079) and its `packages/`
  tree. Three kinds of set exist, in the environment search order of ADR-0201: the user's
  set (`<config dir>/modules/`), each pack's set (`<pack>/modules/`), and the release's set
  (`<exe dir>/../share/p1/modules/`). The loader looks a name up across the sets in that
  order and refuses a name no set holds (`NotInstalled`, which replaces `NotOfficial` and
  `NotInManifest`); a set's manifest is still refused whole when it is malformed or lists one
  name twice.
- **The lock pins, the sets supply.** An environment keeps naming a module by its key; the
  lock entry for that key keeps the five fields of ADR-0087 (`package`, `version`, `digest`,
  `world`, `protocol`) and may now name any publisher's package. The loader verifies the
  bytes it reads against the lock's digest and against the set manifest's digest, and refuses
  on either mismatch; a lock entry whose digest no installed set can satisfy is refused when
  the environment assembles, naming the lock file and the key. The shipped `modules.lock`
  at the repository root may name only `p1/` packages, so a release stays what p1 built;
  packs and the user lock may name anything they install.
- **Installing is explicit and records the digest.** `p1 modules install <package-file>`
  reads a package archive (manifest and component), verifies the manifest's digest against
  the bytes, copies it into the user's set and writes or replaces the lock entry for the
  module key the manifest declares, printing the digest it pinned. Nothing is installed by a
  model, an environment or a network fetch inside p1; a user who wants a module obtains the
  file and installs it. `p1 modules list`, `inspect` and `verify` cover every installed set
  and say which set each package comes from.
- **Trust is the sandbox and the allocation, not the publisher.** An installed module runs
  with exactly the capability allocation of its class (`docs/design/modules/capabilities.md`),
  which the loader now enforces at load time for every set: a component whose imports exceed
  its world's allocation is refused (`ImportsExceedAllocation`), the same check
  `scripts/check-module-boundaries.sh` runs for the repository's own packages at build time.
  A user installing a package accepts that it runs with that allocation and nothing more.
- **No signatures and no registry in this record.** Digest pinning by the installing user is
  the trust rung this record builds; a signed manifest, a publisher key list or a registry
  that maps names to digests is a later record, added when there are publishers to sign. The
  manifest format gains no field here.

## Consequences

- A module built in another repository with the published SDK (#888) loads into a released p1
  after one `p1 modules install`, pinned by digest in the user's lock. The repository's own
  modules keep loading exactly as before; nothing in the release changes shape.
- The loader gains the set abstraction and the load-time allocation check; `LoadError` loses
  `NotOfficial` and `NotInManifest` for `NotInstalled`, `DigestMismatch` names the set. The
  lock parser drops the `p1/`-only rule for user and pack locks and keeps it for the shipped
  lock. `p1 modules` gains `install` and the set column. `docs/design/modules/package.md`
  §"The reserved `p1/` namespace" and "The loader" are rewritten under this record.
- Security boundary unchanged in kind: a third-party module is as confined as an official one
  (WebAssembly sandbox, capability grants, redaction of what reaches the model), and the
  host still never loads caller-chosen bytes. New in degree: the user, not p1's build, decides
  what runs under an allocation. Lock and set files stay user-writable configuration under the
  pinned-directory rules (ADR-0108) so a group-writable set is refused like a credential dir.
- The connectors (#889) become the first external-style packages: built in this repository
  under `connectors/`, installed by the distribution build into the release set under their
  own namespace, which proves the path without a second repository.

## Alternatives considered

- **Keep official-only and ask publishers to fork.** Defeats the owner's goal; every module
  would have to be merged here to load anywhere.
- **Let a lock entry name a path or a URL.** Loses the identity binding ADR-0087 established
  and makes a configuration file a code source; refused.
- **Signatures or a registry now.** Right rung later; today there is no publisher to sign and
  no registry to run, and digest pinning by the installing user already prevents substitution.
- **Namespaces owned through a central list in this repository.** Puts p1 between every
  publisher and their users; a label needs no owner when the digest is the identity.

## Evidence

- The refusals this record replaces: `crates/p1-module-runtime/src/loader.rs`
  (`LoadError::NotOfficial`, `LoadError::NotInManifest`, the `p1/` checks around the manifest
  lookup) and the lock rule in `crates/p1-assembly/src/modules_lock.rs` (`valid_module_name`).
- Set layering follows `environment_dirs()` (user directory, packs, shipped; ADR-0201).
- The build-time allocation check that moves into the loader:
  `scripts/check-module-boundaries.sh` (imports against the frozen capability allocation).
- Acceptance for #887: a package built from `modules/p1-module-fixture` under a non-`p1`
  namespace, installed with `p1 modules install` into a scratch config dir, loads and
  dispatches in a test environment; the same package with one byte changed is refused with
  `DigestMismatch`; a component importing beyond its allocation is refused with
  `ImportsExceedAllocation`.
