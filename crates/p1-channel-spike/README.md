# p1-channel-spike

A spike, not a p1 crate to ship: does a generic channel (one frozen WIT world, every event and
host call by name with a JSON payload, large data by handle, capabilities from manifest ∩ grant)
cost more than p1's typed per-interception WIT?

- WIT: `modules/p1-bindings-channel/wit/channel.wit` (`p1:channel@1.0.0`, world `plugin`).
- Guest: `modules/p1-module-channel-fixture` (wit-bindgen 0.62.0, wasm32-unknown-unknown,
  `wasm-tools component new`, no WASI). Built by `build-guest.sh`, not by
  `scripts/build-modules.sh` (it has no `[package.metadata.p1-module]`).
- Host: `src/lib.rs` (`Plugin::load`, `Plugin::emit`), catalog in `src/catalog.rs`.
- E1/E2: `src/bin/bench-channel.rs`; `bench.sh` builds everything and runs `bench-baseline`
  and `bench-channel` back to back in the dev profile.
- E3: `tests/fail_closed.rs` and the unit test in `src/lib.rs`; E4: the last case there.
- E5: `modules/p1-module-channel-shell` (`p1-shell-guest` over the channel), the `process`
  capability adapter `src/process.rs`, `tests/shell.rs`.

```
crates/p1-channel-spike/bench.sh
crates/p1-channel-spike/build-guest.sh && cargo test -p p1-channel-spike
```
