---
adr: 8
title: rustfmt and clippy installed for the gate
status: accepted
date: 2026-09-19
deciders: lead
supersedes: []
superseded_by: []
sources: [D10, scripts/gate.sh]
---
# ADR-0008: rustfmt and clippy installed for the gate

## Context

D10 (lead): the required gate (fmt check plus `clippy -D warnings`) could not run because
`cargo-fmt is not installed for the toolchain stable`. The fix had to be user-level: the
project rule is no sudo and no apt installs.

## Decision

The `rustfmt` and `clippy` rustup components were installed for the stable toolchain
under `~/.rustup`. Nothing system-wide changed.

## Consequences

The gate's first two steps run. The machine's Rust toolchain carries the extra components,
so a fresh machine must add them the same way before the gate is green.

## Alternatives considered

None recorded.

## Evidence

`../../scripts/gate.sh` runs `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets --locked -- -D warnings`. D10 records the original
error text.
