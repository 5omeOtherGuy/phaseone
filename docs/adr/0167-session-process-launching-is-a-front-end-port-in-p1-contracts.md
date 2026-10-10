---
adr: 167
title: Session process launching is a front-end port in p1-contracts
status: accepted
date: 2026-10-10
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0167: Session process launching is a front-end port in p1-contracts

## Context

ADR-0156 has the ACP router start one session process per `session/new` through a `SessionLauncher` trait and a `Launched` pipe pair defined in `p1-acp`, with the host's `ProcessLauncher` behind it. Starting an isolated session for a workspace is not an ACP matter. Owner decision of 2026-10-10 (#758): "The GUI WILL need it." A GUI server would otherwise depend on `p1-acp`, or copy the launcher.

## Decision

`SessionLauncher`, `Launched` and the two pipe types live in `p1-contracts` (`session_launch.rs`) beside `FrontEndPort` and `SessionHandle` (ADR-0152). The contract is bytes in, bytes out, an exit future and the workspace path; it names no protocol and no front end. The p1-acp router consumes it; the host's `acp_launch.rs` implements it with unchanged ADR-0156 behaviour (this binary with `--workspace <folder> --serve-session`). ADR-0156 is amended by reference, not edited.

`p1-contracts` gains `tokio` with no features, only for the `AsyncRead` and `AsyncWrite` traits of the pipes; `p1-core` already depends on tokio.

## Consequences

- A GUI server composes the same launcher and needs no `p1-acp` dependency.
- `p1-acp` no longer exports `Launched` or `SessionLauncher`; users import them from `p1-contracts`.
- Behaviour, ordering and wire output are unchanged.

## Alternatives considered

- Keep the trait in `p1-acp` and let the GUI depend on it: couples a second front end to ACP.
- Pipe types as `std::io::Read`/`Write`: would force blocking threads for async relays.

## Evidence

`cargo test -p p1-contracts -p p1-acp` and `cargo test -p p1-host acp`; `scripts/check-core-isolation.sh`; `scripts/check-module-boundaries.sh`.
