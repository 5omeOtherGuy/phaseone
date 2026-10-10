---
adr: 152
title: Front ends attach through an ACP-neutral port in p1-contracts
status: accepted
date: 2026-10-10
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0152: Front ends attach through an ACP-neutral port in p1-contracts

## Context

Epic #670 makes `p1 acp` the one door for front ends. On 2026-10-10 the owner decided D7 (ports and adapters): "A new ACP-neutral port in p1-contracts, with a host session handle", where "p1-acp holds the translation *and* the stdio driver; p1-host only composes", "`FrontEnd` stays the host's composition trait", and "p1-contracts never learns ACP" stays true.

`FrontEnd` (`crates/p1-host/src/frontend.rs`) cannot move to p1-contracts. It names host types (`HostDeps`, `Options`, `StallGuard`, `QuestionBridge`, `ShippedPolicy`, `VerifiedSources`, `Completion`), p1-core's `Agent`, p1-workers' types and p1-tui's workflow types. An adapter crate that depends on p1-host to implement it would turn the dependency direction around.

## Decision

`p1_contracts::frontend` holds the port. It uses contracts types only and never names a wire protocol.

- **`FrontEndPort`** is what a front end implements. Outbound, it receives:
  - the parent event sink and the per-worker sink factory;
  - authorization;
  - `background(BackgroundSignal)` — started or ended, kind `Worker | Workflow`, the host's id, and the ordinal of the turn that started it.

  Its `run(&dyn SessionHandle)` drives the session.
- **`SessionHandle`** is what p1-host implements. It is inbound, every method takes `&self`, and every method is Send-capable:
  - `prompt(text, cancel)`
  - `cancel_runs()`
  - `stop_workers()`
  - `drain_inbox(cancel)`
  - `inbox_ready()`, the idle wake a front end races with its next request

  Each method's doc names its user. A method is added only when an adapter needs it.

`p1_host::frontend_port::PortFrontEnd` bridges any port into `FrontEnd`:

- The worker and workflow callbacks become background signals. A worker that `worker_continue` runs again starts again at that turn's `TurnStarted`. A workflow step's worker ends at its step's end, at a fallback that replaces it, or at its run's end.
- The host's session handle keeps the agent behind an async lock. That lets the cancel hooks run beside a turn.

`LineFrontEnd`, `TuiFrontEnd`, p1-tui and `tui.rs` are unchanged.

## Consequences

- p1-acp (#672, #673) depends on p1-contracts only and stays a plug-in adapter. Another protocol can be added as another adapter.
- The bridge is now the place where host callbacks turn into port signals. A new host callback that a front end must see needs a port method, a bridge line and a test.
- Two additions to the issue's sketch: `drain_inbox` takes a cancellation token, so an inbox turn can be cancelled like a prompt; and `inbox_ready` exists, because a front end at idle must hear a worker's notice without polling, as `run_interactive` does.
- Background shell jobs are not background work in this sense. They run as tool calls and report through the inbox, so they produce no signal.
- `ask_user_question` uses the default headless question bridge until an adapter needs a question method.

## Alternatives considered

- Moving `FrontEnd` into p1-contracts. Refused: it would drag host, core, worker and TUI types into contracts.
- An adapter implementing `FrontEnd` from p1-host directly. Refused: p1-acp would depend on p1-host, and D7 forbids that.
- `&mut self` session methods. Refused: a cancel request has to reach the session while a prompt's future holds it.

## Evidence

- `cargo test -p p1-contracts`: the ports are object-safe and Send.
- `cargo test -p p1-host --test frontend_port` covers the bridge with a fake adapter on scripted providers:
  - cancel reaches both hooks;
  - a worker and a workflow run each signal start and end once, with the turn that started them;
  - a continued worker signals a second start and end;
  - `drain_inbox` runs the inbox turn that their notices open;
  - a background shell job signals nothing.
