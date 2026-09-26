# Module toolchain: pins and the lint policy for generated bindings

Status: published freeze item 11 of the WebAssembly boundary (ADR-0071): the toolchain pins and
the lint policy for generated bindings under `unsafe_code = "forbid"`. The pins are data in
[`modules/toolchain.pins`](../../../modules/toolchain.pins), checked by
[`scripts/module-toolchain.sh`](../../../scripts/module-toolchain.sh); the lint policy and the
check that enforces it are in [`capabilities.md`](capabilities.md#the-unsafe-policy-freeze-item-11).
Where the toolchain runs is [ADR-0077](../../adr/0077-builds-on-the-stream-boxes.md); what a
guest may be is [ADR-0081](../../adr/0081-native-foundation-and-runtime-components.md).

## The pins

`modules/toolchain.pins` is a `KEY=value` file. The script parses it line by line and never
sources it, so a stray command in it cannot run.

| Pin | Value | What it pins | Checked against |
|---|---|---|---|
| `RUST_MIN` | `1.96.0` | the oldest Rust that builds the host runtime and the guests: the `rust-version` of wasmtime 49 | `rustc -V`: an older compiler fails the check |
| `WASM_TARGET` | `wasm32-unknown-unknown` | the guest target (D-XO-4 on S0-Q9, below) | the target's std in the rustc sysroot |
| `WASMTIME` | `49.0.1` | the host runtime | the root `Cargo.lock` |
| `WASMTIME_FEATURES` | `runtime,cranelift,component-model,async,std` | wasmtime's features, as a set, with default features off | the `wasmtime` dependency of [`crates/p1-module-runtime/Cargo.toml`](../../../crates/p1-module-runtime/Cargo.toml) |
| `WIT_BINDGEN` | `0.62.0` | the guest-side bindings generator | [`modules/Cargo.lock`](../../../modules/Cargo.lock) |
| `WASM_TOOLS` | `1.259.0` | the build tool that componentizes, extracts worlds and validates | `wasm-tools --version` on `PATH` |

The wasmtime feature set is the bounded dependency set of the owner's answer Q7. It leaves out
`cache`, so no compiled component is ever read back from a cache the loader did not verify, and
`wat` and WASI, so modules arrive as binary components and get only p1's own imports
([`package.md`](package.md#the-loader-freeze-item-6)). `wit-bindgen` is built with `macros`,
`realloc` and `std` only ([`modules/Cargo.toml`](../../../modules/Cargo.toml) says why each).

`scripts/module-toolchain.sh --check` prints one `key: value` line per pin with its result, a
`wit:` line with the SHA-256 of every file of [`modules/wit/`](../../../modules/wit/), and ends
`module-toolchain: OK` (exit 0) or with the failures (exit 1). That record is the evidence of the
"Recorded compatible toolchain" row of S0's definition of done.

### The guest target (D-XO-4)

The stream brief's freeze list names `wasm32-wasip2`; the owner's programme answered S0-Q9 with
D-XO-4 instead: guests build for `wasm32-unknown-unknown` and `scripts/build-modules.sh`
componentizes the core module with `wasm-tools component new`, with no WASI adapter. A guest has
no std I/O by design, so a built component imports only `p1:module` interfaces; the build, the
loader and the boundary check each refuse a `wasi:` import
([`package.md`](package.md#the-guest-target-s0-q9),
[`capabilities.md`](capabilities.md#why-wasi-is-always-refused-d-xo-4)). The pin took effect with
S0.5 (PR #252, merge commit `4872fd78`).

### Where the toolchain is installed

`scripts/gate.sh` runs `scripts/module-toolchain.sh --check`, `scripts/build-modules.sh --all`
and `scripts/check-module-boundaries.sh` before the tests, on the stream boxes and in CI alike.
The workflows [`.github/workflows/ci.yml`](../../../.github/workflows/ci.yml) and
[`build.yml`](../../../.github/workflows/build.yml) install the stable toolchain with the
`wasm32-unknown-unknown` target and the `wasm-tools` version read from `WASM_TOOLS` in the pins
file, so the pin is the one place that version is written. A missing target std, a missing
`wasm-tools` or a version that differs from its pin fails the check; nothing is skipped.

## The lint policy for generated bindings

The root workspace and the module workspace both set `[workspace.lints.rust] unsafe_code =
"forbid"`, and every crate inherits it with `[lints] workspace = true` — module packages,
bindings crates, shared guest crates under `crates/` (S0-R3,
[`package.md`](package.md#shared-guest-logic-s0-r3)) and the host crates `p1-module-runtime`,
`p1-module-protocol` and `p1-module-tests`. The module workspace is a separate Cargo workspace
([`modules/Cargo.toml`](../../../modules/Cargo.toml)) so the guest crates, which link
`wit-bindgen` and build for the wasm target only, never enter the host's dependency graph.

Generated bindings keep the prohibition by the bindings-crate pattern
([`package.md`](package.md#the-binding-crate-pattern)): `wit_bindgen::generate!` runs in a
bindings crate (`modules/p1-bindings-tool`, `modules/p1-bindings-workflow-decision`) with
`pub_export_macro`, and a component crate expands the exported `export!` macro. The macro's
expansion needs `unsafe`, and a macro defined and expanded in the same crate fails the lint
there, so definition and expansion live in different crates and every crate still compiles under
`forbid`. On the host side the runtime uses wasmtime's dynamic `Val` forms where a WIT record or
variant crosses, because the derived typed forms expand to `unsafe impl`s
([`capabilities.rs`](../../../crates/p1-module-runtime/src/capabilities.rs)).

`scripts/check-module-boundaries.sh` enforces the policy: a crate that does not inherit the
prohibition, a workspace that does not set it, or handwritten source that uses `unsafe` is a
finding; a crate whose code is generated by `wit_bindgen::generate!` or
`wasmtime::component::bindgen!` is reported as generated, not failed
([`capabilities.md`](capabilities.md#the-unsafe-policy-freeze-item-11)). The stop rule's
question Q8 — whether generated bindings compile under the prohibition at all — was not
triggered: they do, with this pattern.
