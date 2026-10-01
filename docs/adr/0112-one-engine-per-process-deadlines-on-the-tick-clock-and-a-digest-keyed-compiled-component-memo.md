---
adr: 112
title: One engine per process, deadlines on the tick clock and a digest-keyed compiled-component memo
status: proposed
date: 2026-10-01
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0071, ADR-0082, ADR-0087, issue #501, freeze items 4 and 6]
---
# ADR-0112: One engine per process, deadlines on the tick clock and a digest-keyed compiled-component memo

## Context

Issue #501: every `Loader::new` builds its own wasmtime `Engine`, starts its own 10 ms epoch
ticker thread and compiles each component it loads from bytes (`Component::from_binary`). One
process builds many loaders: each catalog build, each worker's and each reload's `HostDeps`, the
front end's policy, every in-process test run. The same verified bytes are compiled again and
again; in the gate's longest job the p1-host tests spend most of their time doing that
(measured in #501: 1191 s of tests, 93 % in p1-host).

A component is compiled for one engine and only runs on it, so sharing compiled code needs a
shared engine. A shared engine shares its epoch: the ticker of every loader would advance it,
and `Epochs::interrupt` (a cancelled call bumps the engine's epoch so a guest in a CPU loop
reaches its callback) acts on every guest of the engine. The execute and provider paths already
count their deadlines on the tick clock beside the engine's epoch, so an extra bump only makes
their callback run once more. Two paths still use the engine's own epoch as their deadline
(`epoch_deadline_trap` with a tick budget): the restricted inspection path (`restricted.rs`,
`RESTRICTED_DEADLINE_TICKS`) and the workflow decisions (`workflow_decision.rs`). On a shared
engine every interrupt or extra ticker anywhere in the process would shorten them.

ADR-0087 states "verify, then compile the same bytes … no compiled cache is ever deserialized".
This decision keeps both; it adds an in-memory memo of what this process itself compiled.

## Decision

- **One engine per process.** `Loader::new` takes the process's one engine (built by `engine()`
  on first use) and its one epoch clock, advanced by one ticker thread started on first use.
  Every loader, and so every module of every catalog build, worker and reload, runs on them.
- **Deadlines count ticks, never raw epochs.** The restricted path and the workflow decisions
  arm the same tick-count epoch callback as the execute and provider paths: the deadline is a
  count on the shared tick clock, so an interrupt or another loader never brings it closer. A
  bump that is not a tick makes the callback run once and continue.
- **A compiled-component memo keyed by digest.** The shared engine keeps the components it has
  compiled, keyed by the SHA-256 digest of the bytes the loader verified. `Loader::load` still
  reads the file once, hashes it and compares the digest with the manifest before anything
  else; only then does it look the digest up, and compiles those bytes on a miss. The memo
  holds only the compiled component: the name, kind, capabilities, identity and the import
  check come from each load's own manifest entry, every time. Nothing is serialized,
  written or deserialized; the memo lives in memory for the life of the process.
- **Tests keep their own clock.** `Loader::with_manual_epochs` builds a private engine and
  clock and skips the memo, so a test that advances epochs by hand moves only its own guests.

## Consequences

- One compile per distinct component per process instead of one per load; one ticker thread
  per process instead of one per loader.
- A cancellation now wakes the epoch callback of every running guest in the process once; each
  callback reads the tick clock and continues. The restricted backstop and the decision
  deadlines no longer lose a tick per interrupt anywhere.
- Compiled components stay in memory until the process ends, one per distinct digest loaded;
  a long session that reloads many different releases keeps each one's compiled code.
- The engine's configuration is the process's: a test cannot build a loader with another
  `Config` (none does).

## Alternatives considered

- Test-only reuse (one loader per test process, injected through `HostDeps`): helps the gate
  only, needs a new seam, and leaves production compiling the same bytes once per loader.
- Keeping one engine per loader and memoizing per loader: a catalog build already shares one
  loader; the repeats are across loaders (workers, reloads, policies), which a per-loader memo
  cannot reach.
- A memo keyed by package name or path: rejected; the same name can carry other bytes after an
  in-place replacement. Only the digest of the bytes just verified names what was compiled.
- Leaving the restricted and decision deadlines on the engine's own epoch: rejected; on a shared
  engine every interrupt and every extra ticker in the process would shorten them.
- A compiled cache on disk: that is #501 step 3, decided in its own ADR; this one adds nothing
  that is read back from outside the process.

## Evidence

`cargo test --locked -p p1-module-runtime loader::tests` (`an_interrupt_never_shortens_a_deadline`,
`guests_on_one_clock_keep_their_own_deadlines`, `loaders_share_one_engine_and_compile_a_digest_once`
with the per-entry name, grants, variant and import check, and
`a_manual_epoch_loader_has_its_own_engine_and_compiles_itself`); the manual-epoch cases of
`cargo test --locked -p p1-module-tests --test cancellation` are unchanged. The gate's
`test (workspace without p1-module-tests)` job time before and after is recorded on #501 and in
the pull request.
