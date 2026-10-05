---
adr: 113
title: Releases ship ahead-of-time compiled components pinned by their own digest
status: accepted
date: 2026-10-01
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [ADR-0079, ADR-0082, ADR-0087, ADR-0112, issue #501, freeze items 6 and 11]
---
# ADR-0113: Releases ship ahead-of-time compiled components pinned by their own digest

## Context

Every p1 process compiles the components it loads from their bytes (ADR-0087, freeze item 6:
"no compiled cache is ever deserialized"). ADR-0112 compiles each distinct component once per
process, but the first compile still sits on every start: `p1 env show claude` took 9.4 s on
release `main-7bae2ecd92aa` and 9.5 to 12.0 s on `main-765e9cddba34` on the workstation (issue
#501), against PLAN §10's target of a first assembly within 3 s.

wasmtime can load a component compiled ahead of time (`Engine::precompile_component`, then
`Component::deserialize`), which skips compilation. `Component::deserialize` is an `unsafe`
function: wasmtime trusts the bytes to be its own compiler's output, so the caller must make
sure they come from a trusted source. p1 forbids unsafe code in every crate (`unsafe_code =
"forbid"`, freeze item 11), and ADR-0079 says a release never ships a `.cwasm`.

The owner decided both points for this issue: on 2026-09-30, in the question dialog on the
plan, "Yes, with fingerprints (Recommended)" (ship compiled components, each pinned by its own
digest in the release manifest), and on 2026-10-01, in the question dialog of the #501 session,
"Allow one call (Recommended)" (p1-module-runtime may make the one `unsafe` call).

## Decision

- **A release ships a compiled copy of each component.** The release staging
  (`scripts/stage-release.sh`) runs the binary it ships, `p1 modules precompile --root <staged
  modules>`, which writes `packages/<package>/<package>.cwasm` beside each `<package>.wasm`
  with `p1_module_runtime::precompile`: the runtime's own engine configuration with the
  explicit target `x86_64-unknown-linux-gnu`, so the compiled code needs no CPU feature beyond
  baseline x86-64 whatever the build machine has. `scripts/release-manifest.py` lists the file
  under `packages` like every other and names it in the component's entry as
  `precompiled: {path, digest}`; a release package without its copy is refused. A development
  manifest (`scripts/build-modules.sh`) names none, so development builds and the tests that
  use them keep compiling every component.
- **Trust root.** Only the module root installed beside the running binary may supply native
  compiled copies: `<current_exe parent>/../share/p1/modules`, the layout produced by
  `install.sh` and `stage-release.sh`. The host derives this from `std::env::current_exe`
  and passes it explicitly to `Loader::for_installation`; the runtime has no global trust
  root or environment lookup. `Loader::new` and manual-epoch loaders are compile-only.
  Other roots, including `modules inspect --root`, configured/user module directories and
  development/test roots, ignore `precompiled` for loading and compile the verified component.
- **Loading.** The loader verifies the component exactly as before (one read, the digest,
  the header). For both `.wasm` and `.cwasm`, every path component below the root is opened
  relative to an already-open directory handle with `O_NOFOLLOW`; no parent or final symlink
  is allowed. The final open handle is checked with `fstat` for a regular file, then its bytes
  are read, hashed and used without reopening the path. A path violation refuses the load.
  Only at the trusted installation root, when the entry names a compiled copy, the loader
  compares the SHA-256 of those bytes with the manifest's `precompiled` digest and refuses
  the load on a mismatch, before anything is deserialized. It deserializes
  the in-memory bytes it hashed with `Component::deserialize`, never `deserialize_file`. A
  copy wasmtime refuses (another wasmtime version, another engine configuration, a CPU feature
  the host lacks) is not an error: the verified component is compiled instead. A manifest that
  names a copy the release does not hold is a refused load, as a missing component is. The
  import check, the identity and the grants are the component entry's, as for a compiled load,
  and ADR-0112's memo keeps one built component per component digest and trust domain,
  so an untrusted root never reuses a deserialized component or reports it as locally compiled.
- **One unsafe call.** `p1-module-runtime` sets `unsafe_code = "deny"` instead of inheriting
  the workspace's `forbid`, and allows it on one function, `loader.rs`'s `deserialize`, whose
  body is the one `unsafe` block with its safety argument. Every other crate keeps `forbid`;
  `scripts/check-module-boundaries.sh` accepts this crate only with `deny` and exactly one
  `unsafe` use, in that file, and reports any other.
- **Verification.** `p1 modules verify` hashes the compiled copy against its digest as it
  hashes the component; `p1 modules inspect` says whether the component was loaded from the
  release's copy or compiled at load. The installer verifies the `.cwasm` like every package
  file (size and sha256 from `packages`).

This amends ADR-0079 (a release ships its compiled copies), ADR-0087 and freeze item 6 (the
loader deserializes the release's own verified copy) and freeze item 11 (the one exception to
the unsafe policy).

## Consequences

- An installed release loads its components without compiling them; the release job spends
  the compile time once, when it stages.
- Trust requires both the executable-derived installation root and the digest check. Only
  there does the manifest that admits `.wasm` also authorize `.cwasm`: someone able to rewrite
  that installation's manifest and compiled copy could equally replace its p1 binary. This
  argument does not apply to arbitrary module roots; their manifests never authorize native
  deserialization, even when their compiled-copy digests match. Symlink escapes and bytes
  differing from the installed manifest are refused before deserialization.
- A copy wasmtime refuses costs a compile, never a failure: a p1 built with another wasmtime
  or configuration still runs the release's components. A host lacking a CPU feature cannot
  occur for baseline code, but would be the same fallback.
- The share archive grows by one compiled file per package.
- Development builds and CI's tests keep the compile path, so it stays exercised; the
  installed-release tests stage with `stage-release.sh` and so exercise the compiled copies.

## Alternatives considered

- Compiling only the modules an environment selects (issue #501 option 3): fewer compiles,
  but the first assembly still compiles every module it uses.
- A compiled cache written on the user's machine on first use: needs a writable cache
  directory and a second trust decision for files p1 wrote earlier; rejected.
- Compiling for the build machine's CPU: smaller and faster code, but a copy compiled on a
  newer CPU would be refused on an older one and fall back to compiling every time; rejected
  for an explicit baseline target.
- Keeping `forbid` and isolating the call in a new crate: the call is still unsafe code of
  p1, and a crate for one function adds a dependency edge without changing the trust; rejected.
- No precompiled copy at all (the owner's other option): leaves the start-up time.

## Evidence

`cargo test --locked -p p1-module-runtime` (`a_verified_compiled_copy_is_deserialized`,
`a_compiled_copy_with_another_digest_is_refused_before_deserializing`,
`a_compiled_copy_wasmtime_refuses_falls_back_to_compiling`, the manifest's
`reads_a_precompiled_copy_and_refuses_a_malformed_one`); `cargo test --locked -p p1-host`
(`verify_hashes_the_compiled_copy_against_its_own_digest`, the `precompile` parse case);
`python3 scripts/test_release_manifest.py`, `scripts/test_stage_release.py`,
`scripts/test_install.py` and `scripts/test_check_module_boundaries.py`; the installed-release
tests of `p1-module-tests`. The before and after start-up times are on issue #501 and in the
pull request.
