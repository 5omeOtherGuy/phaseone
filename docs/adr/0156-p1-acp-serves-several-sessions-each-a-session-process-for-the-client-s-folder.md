---
adr: 156
title: p1 acp serves several sessions, each a session process for the client's folder
status: accepted
date: 2026-10-10
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0156: p1 acp serves several sessions, each a session process for the client's folder

## Context

ADR-0154 scoped the first `p1 acp` to one session per process. A `session/new` had to name p1's `--workspace` as its `cwd`. Multi-session was left to a later child of epic #670 (D8).

Issue #690 adds three requirements:

- More than one `session/new` per process. Each session has its own agent, cancel token, hold and approvals.
- `session/close`.
- A `sessionId` on every update and request.

The owner's decision of 2026-10-10 (#690): "each session works in the folder the client names". The `cwd` of `session/new` is an existing absolute path. p1's sandbox, access and approval rules apply to that folder exactly as with `p1 --workspace <cwd>`, and `--workspace` becomes the default. The ACP TCK must report CONFORMANT. dsh's ACP agent serves several sessions per process and `session/close`, and is the floor (#690).

A host process owns one agent. `run_with_front_end` keeps process-wide state for that one session:

- the model-switch context;
- the parent's job registry;
- the user-question gate;
- the worker member scopes;
- the workflow service on `HostDeps`.

## Decision

`p1 acp` is a router. It holds no agent. It answers `initialize` itself, and for each `session/new` it starts one **session process**. That process is this same binary with the same run options, the session's folder as `--workspace`, and the internal flag `--serve-session`.

- **The session process** is ADR-0154's single-session driver, unchanged. It assembles its own agent for its folder, and it has its own sandbox, cancel token, hold, approvals, workers and workflow runs.
- **The folder:**
  - The client's `cwd` must be an existing absolute directory, else the request gets invalid params.
  - A `session/new` without `cwd` takes the operator's `--workspace`. Without `--workspace` it gets invalid params, as ACP requires `cwd`.
- **Ids and order:**
  - The router gives each session its own id and rewrites `sessionId` in both directions.
  - It relays permission requests to the client under the session's id.
  - One task relays each session's output in order, so a session's updates reach the client before its responses.
- **`session/close`** closes the session process's input. The process ends its session as on EOF: it cancels the running prompt, which answers `cancelled`; stops its workflow runs and workers; and exits. Close then answers `{}`. `agentCapabilities.sessionCapabilities.close` is advertised.
- **Client EOF** closes every session the same way. The router exits once every session process has exited.
- **Journals:** `p1 acp` refuses `--session` and `--resume`. Every session is kept in memory, and one journal file cannot hold several sessions. Session history over ACP is #62.
- **The seams:** the router reaches processes through the `SessionLauncher` port in p1-acp. The host's adapter (`crates/p1-host/src/acp_launch.rs`) starts real processes; the tests start the driver in process. p1-core and p1-contracts do not change.

## Consequences

- **Isolation.** Sessions cannot cross. Each one has its own process, agent, sandbox root and approvals. A crash takes down one session, not the others.
- **Cost:**
  - Every session pays a host start, including loading the module set: a few seconds in a debug build.
  - Every session uses its own memory.
  - Sessions share no provider connection or cache.
- **Host unchanged.** p1-host's ownership of sessions is untouched: still one agent per host process. The router composes nothing.
- **ADR-0154 amended.** Its "one session per process" and its `cwd` rule are replaced by the above. Its wire surface and hold rule stand.

## Alternatives considered

- **Several agents in one host process.** This needs every piece of process-wide state above to move to a per-session owner, so `run_with_front_end` could be composed once per session. It means a large change to the host's composition root that the issue does not ask for. Kept as the path if session start cost becomes the bottleneck.
- **Accept any `cwd` but keep tools bound to `--workspace`.** Rejected by the owner's decision: the session works in the client's folder.

## Evidence

- `cargo test -p p1-acp --test router`:
  - two sessions with their own ids, folders and updates;
  - an approval routed to its own session;
  - a cancel that leaves the other session running;
  - a close that cancels a held prompt and stops its work;
  - the `cwd` rules.
- `cargo test -p p1-host acp`: the CLI and the session process's arguments.
- The #690 pull request:
  - the TCK report;
  - a two-session run of one `p1 acp` against a real provider;
  - acpx runs of two named sessions.
