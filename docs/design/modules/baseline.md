# Cold and warm cost of the module runtime (baseline)

Status: published with slice S0.8 (the DoD row "Cold/warm cost recorded"). This document is a
record, not a check: it states what the runtime costs on one box at one commit and carries no
target, no bound and no threshold. The gate does not run the suite, and no code compares a
reading with a number. The programme lead compares records, and the owner's plan holds whatever
bound is wanted.

The suite is [`scripts/bench-modules.sh`](../../../scripts/bench-modules.sh) (`--suite baseline`,
its only suite; anything else exits 2). It builds the module packages if their outputs are
missing, builds [`bench-baseline.rs`](../../../crates/p1-module-tests/src/bin/bench-baseline.rs)
in the debug profile the gate uses, runs that binary under `/usr/bin/time -v`, and writes one
record file — the bench binary's own lines, the max RSS, the sizes of the three binaries, the
storage of the two build trees, the commit, `rustc -V` and the module toolchain pins — to
`.worker-scratch/bench-baseline-<shortsha>.txt` (scratch, never committed) or to `--out`. It
prints the record and ends with the line naming its path.

## What a reading is

| Key | What the reading includes | What it excludes |
|---|---|---|
| `cold_engine_creation` | building a wasmtime engine with the runtime's configuration: the component model, epoch interruption and fuel (the runtime's own `engine()`) | compiling or running anything |
| `cold_loader_creation` | a `Loader` over the harness's release manifest, read back from its file; it builds the engine the module is compiled with and starts the epoch ticker thread | reading or compiling the component |
| `cold_load` | the first `Loader::load`: reading the component file, computing its SHA-256 and comparing it with the manifest digest, compiling those same bytes with `Component::from_binary`, and checking the component's imports against the manifest's grants | instantiating the module, linking capabilities, running it |
| `cold_tool_construction` | `wasm_tool` over the loaded module: the capability linker, `instantiate_pre`, the second instance that carries the restricted path with its `declaration` read, and the executor task | running a call |
| `cold_first_execute` | the first `execute("echo:hi")`: the executor's channel round trip, a fresh `Store` and a fresh instance from the pre-linked component, the call and the decode of its outcome | the module's load, link and declaration |
| `warm_execute_median`, `warm_execute_max` | the median and the maximum of `warm_calls` `execute("echo:hi")` calls on the loaded tool, each with its own `Store` and instance (the runtime poisons nothing between calls) | the cold path, which the tool is already past |
| `warm_describe_median`, `warm_describe_max` | the median and the maximum of the same number of synchronous `describe` calls on the restricted path: a call on the adapter's own thread, no capability linked, a tight fuel budget, and the instance built once unless a call trapped | everything the asynchronous path costs; no future, no executor, no runtime |
| `max_rss` | `/usr/bin/time -v` of the whole bench process: the engine, the compiled component, the fixture's bytes and both instances | anything the host's own process holds |
| `fixture_wasm`, `bench_binary`, `libp1_module_runtime_rlib` | the file sizes of the shipped fixture component, the bench binary the gate's profile produces, and the runtime library | anything else in the build tree |
| `modules_target`, `cargo_target` | `du -sb` of the module workspace's build directory and of this checkout's Cargo target directory (`cargo metadata` names it) at the end of the suite's build step | the storage this suite alone would need: the trees accumulate every build of the checkout |

The bench binary checks every measured call produced the fixture's answer, so a reading is never
taken on a path that did not work; a wrong answer fails the suite (exit 1) instead of being timed.
It asserts nothing about the time itself.

## What the numbers are not

- They are wall-clock facts of one box, one commit and one build profile. The suite prints the
  commit and the toolchain pins it recorded with them; two records are comparable only when that
  provenance matches.
- The compile behind `cold_load` is the crate's own debug-profile wasmtime: it is the profile the
  gate builds and tests, not what a release host runs. A release build's compile cost is a
  different number, and this document does not estimate it.
- The cold readings are one sample each (a first time cannot be repeated in one process), and the
  warm readings are one run of a fixed number of calls on a 2-CPU box with no repetition across
  processes. The maximum is a single sample and carries host scheduling; the median is the robust
  figure of the pair.
- The readings are about the fixture component built at that commit. Another module's numbers are
  another module's.

## The recorded values

Recorded on box wasm-s0 (2 CPUs, 7 GB RAM, Linux x86_64) by one run of
`scripts/bench-modules.sh --suite baseline` at commit `210b724f2d7bbbe67de74b54abf7f7ec6c75c849`
on 2026-09-25T23:37:48Z (`date -u`), with `rustc 1.98.1`, `wasmtime 49.0.1`,
`wit-bindgen 0.62.0` and `wasm-tools 1.259.0`, as `scripts/module-toolchain.sh --check` pinned
them. The record file of that run is `.worker-scratch/bench-baseline-210b724.txt` in the
checkout it ran in; the commit above is the one carrying the suite and the bench binary, and this
document lands in the commit after it.

| Measurement | Reading |
|---|---|
| `cold_engine_creation` | 0.392 ms |
| `cold_loader_creation` | 0.280 ms |
| `cold_load` | 2875.810 ms |
| `cold_tool_construction` | 3.174 ms |
| `cold_first_execute` | 11.065 ms |
| `warm_execute_median` (50 calls) | 0.621 ms |
| `warm_execute_max` (50 calls) | 7.709 ms |
| `warm_describe_median` (50 calls) | 0.023 ms |
| `warm_describe_max` (50 calls) | 0.106 ms |
| `max_rss` | 35468 kB (34.6 MiB) |
| `fixture_wasm` | 45761 bytes |
| `bench_binary` (debug profile) | 73717456 bytes |
| `libp1_module_runtime_rlib` | 8838916 bytes |
| `modules_target` | 48063 bytes |
| `cargo_target` | 1609523275 bytes |

So the fixture's first, cold pay is dominated by compiling the component, the loaded tool then
costs a fraction of a millisecond per call, and inspection on the restricted path costs tens of
microseconds. Nothing here changes when the module is called again: a warm call costs a `Store`,
an instance and the call.
