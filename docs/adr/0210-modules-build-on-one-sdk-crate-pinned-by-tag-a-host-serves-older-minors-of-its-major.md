---
adr: 210
title: Modules build on one SDK crate pinned by tag; a host serves older minors of its major
status: accepted
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0071, ADR-0081, ADR-0113, ADR-0209, issue #886, issue #888]
---
# ADR-0210: Modules build on one SDK crate pinned by tag; a host serves older minors of its major

## Context

Owner, 2026-10-11, for wave 3 of the repository split (#886): "I want modules developed outside
this repository. The whole idea is that everybody can develop modules." ADR-0209 lets p1 load a
package of any publisher once a user installs it. A publisher still had nothing to build one
with. The WIT worlds (`modules/wit/`), the generated bindings (`modules/p1-bindings-*`), the
value protocol (`p1-module-protocol`), the toolchain pins (`modules/toolchain.pins`) and the
build (`scripts/build-modules.sh`) all assumed a crate inside this repository's module
workspace. Nothing said which versions a module may be built against and still load.

## Decision

- **Versions.** The WIT package (`p1:module@MAJOR.MINOR.PATCH`, now 1.0.0) and the value
  protocol (`PROTOCOL_VERSION`, now 1.0) each follow one rule. A module's manifest names the
  world and the protocol it was built with. A host serves a module whose major equals its own
  and whose minor is no greater than its own; a patch is never compared. A minor adds what an
  older module never uses; anything an existing module could notice is a major (`protocol.md`,
  "Version rule"). The loader applies the rule to the manifest's `world` and `protocol` and to
  every `p1:module` import of the component (`compatible_version`).
- **One SDK crate.** A module author depends on `p1-module-sdk` (`modules/sdk/`) and nothing
  else from p1. It has one feature per module class (`tool`, `provider`, `context-policy`,
  `authorization-policy`, `workflow-decision`) that re-exports that world's existing bindings
  crate, and a `protocol` feature that re-exports `p1-module-protocol` and `serde_json`. A
  module exports its implementation with `<world>::export!(T with_types_in p1_module_sdk::<world>)`,
  so the macro that needs `unsafe` still expands outside the crate that defines it, and the
  module keeps `#![forbid(unsafe_code)]` (ADR-0113). The crate's version is the WIT package's.
- **Published as a tagged git dependency.** The SDK is consumed from this repository by tag:
  `p1-module-sdk = { git = "https://github.com/5omeOtherGuy/phaseone", tag = "module-sdk-v1.0.0" }`.
  A tag `module-sdk-v<version>` is pushed on the main commit that changes the SDK, WIT or
  protocol. Publishing to crates.io is a later step, taken when the bindings crates and
  `p1-contracts` are worth a published name each. Nothing here depends on it.
- **The same build outside the repository.** `scripts/build-modules.sh --external <crate dir>
  --output <dir>` builds one package outside this repository with the same steps and checks as
  p1's own packages: pinned target, componentized, validated, world and imports extracted, no
  `wasi:`, manifest fields and class allocation checked. The difference is that the namespace
  must be the publisher's and never `p1/`. `scripts/pack-module.py` packs the result into the
  `.p1pkg` archive `p1 modules install` reads (ADR-0209).
- **A template module.** `docs/design/modules/template/` is a crate a publisher copies: the
  git dependency, the manifest table, `build.sh` and `conformance.sh`, `rust-toolchain.toml`,
  and a CI workflow that builds, checks and attaches the archive to a `v*` release. Its scripts
  run phaseone's own scripts from the checkout cargo fetched for the SDK, so a module is built
  and checked by the code of the very tag it pins.
- **Author conformance.** `crates/p1-module-tests/tests/author_conformance.rs` checks any archive
  named by `P1_CONFORMANCE_PACKAGE`. It installs the archive with p1's install path, loads it
  through the lock and the installed sets, then checks it by class. A tool is inspected on the
  restricted path (`p1_module_runtime::inspect_tool`): its declaration and protocol answers. A
  provider runs the whole `p1-provider-conformance` suite over its route, profile and SSE
  fixtures, p1's own fixtures when its route speaks one of p1's dialects. Without the variable,
  the gate runs the same checks on p1's fixture tool and chat provider republished as
  `acme/*`.

## Consequences

- A module built in another repository with only the SDK dependency loads into a released p1
  after `p1 modules install`, and its author can run p1's checks on it before publishing.
- A WIT or protocol minor bump must keep every older minor's module loading: wasmtime links an
  older minor's import name to the host's definitions by semver, and the loader compares
  imports by interface at any served version. A major bump refuses every older module and needs
  its own record.
- The SDK tag is a release of this repository's module interface. A change to `modules/wit/`,
  `p1-module-protocol` or the bindings that a module can notice changes the SDK's version and
  takes a new tag.
- Running the conformance suite builds p1's test harness from the SDK's checkout, which takes
  minutes the first time. A module repository's CI caches it.
- `p1_module_runtime::inspect_tool` is a public inspection API. It reports what `WasmTool`
  hides behind its worst-case answers, and changes nothing for sessions.

## Alternatives considered

- **Publish the six bindings crates and the protocol separately.** More names to version and
  pin for every author. One facade with features is the fewer-names choice.
- **crates.io now.** It would need `p1-contracts` and the bindings crates published with stable
  names first. The git tag gives the same pinning today; recorded as the later step.
- **An exact version match.** It would make every host minor release orphan every module built
  before it. The semver rule is what wasmtime's linker already supports.
- **A standalone build script in the template.** It would copy `build-modules.sh`'s checks into
  every module repository, where they would drift. Running the pinned tag's own script keeps
  one implementation.

## Evidence

- `docs/design/modules/template/`, copied to a scratch directory outside the workspace with
  the dependency pinned to this change's commit: `./build.sh` fetched the SDK by git and built
  `acme/echo` (89186 bytes, `build-modules: acme-module-echo ok sha256:b78abbee…`).
  `p1 modules install` pinned it in a scratch configuration, and `./conformance.sh` passed on
  it.
- `cargo test -p p1-module-tests --test author_conformance` (the gate's run on `acme/fixture`
  and `acme/chat`), `cargo test -p p1-module-runtime a_version_runs_on_its_major_up_to_the_host_minor`,
  and `python3 scripts/test_build_modules.py` (external mode).
