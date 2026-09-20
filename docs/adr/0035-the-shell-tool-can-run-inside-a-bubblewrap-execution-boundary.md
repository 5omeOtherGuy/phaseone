---
adr: 35
title: The shell tool can run inside a bubblewrap execution boundary
status: accepted
date: 2026-09-20
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/tools.md, docs/review-2026-09-20-dispositions.md, crates/p1-tool-shell/src/lib.rs, crates/p1-tool-shell/tests/sandbox.rs, crates/p1-host/tests/sandbox.rs]
---
# ADR-0035: The shell tool can run inside a bubblewrap execution boundary

## Context

`--yes` permits every tool call, and authorization can only decide WHETHER a command runs,
not what it does once running. In the first live run a worker installed Python packages into
the user's home; the prompt rule added afterwards is a request, not a boundary. The reviewer's
plan amendments (2026-09-20, item 3) state it plainly: per-path grants cannot constrain shell
commands without an enforceable execution boundary, and unattended or concurrent dogfooding
should not start without one. The unsandboxed tool also cannot reach a process that leaves its
process group.

## Decision

`p1-tool-shell` gains an optional sandbox built on bubblewrap (unprivileged user namespaces;
nothing installed, nothing as root): the whole filesystem read-only, a private `/tmp`, the
home directory hidden except an allow-list of toolchain directories (read-only), the runtime
directory hidden, cargo credential files masked AFTER any writable bind, extra writable paths
by explicit flag, the workspace writable, and a PID namespace so every descendant dies with
the command. The mount ORDER is part of the contract (tools.md). The host selects it with
`--sandbox workspace` and `--sandbox-write PATH`; it applies to the parent's and every
worker's `shell`; an unusable sandbox fails assembly before any model call. The default stays
`off` in this increment and is revisited after dogfooding, which always uses the sandbox.

## Consequences

- An agent running with `--yes` can no longer write outside its workspace or read the user's
  credential files through the shell. The file tools were already confined.
- Dogfooding runs in disposable CLONES: a git worktree keeps its metadata outside the
  directory, where the sandbox makes it read-only.
- Not covered: the network stays open, and commands inherit the host's environment variables
  (issue #4). Linux only; CI runners that forbid user namespaces skip the real-bwrap tests
  with a printed SKIP, so the local gate is what proves them.
- The description the model sees says what the boundary is, so a failed install is understood
  rather than retried.

## Alternatives considered

- Prompt rules only: what failed.
- Per-path authorization grants: cannot see inside a command.
- A deny-list of secret paths instead of hiding the home: never complete.
- Containers or a VM: heavier than this machine and this harness want; bubblewrap is present
  and needs no daemon.
- Sandbox on by default now: would break runs on machines without bubblewrap before there is
  any experience with it; deferred, not rejected.

## Evidence

`cargo test -p p1-tool-shell --test sandbox` (17 tests with real bwrap: inside/outside writes,
hidden home and secrets also through a symlinked home, private /tmp, writable paths, the token
mask under a writable `.cargo`, hidden runtime dir, a `setsid` child killed on cancel, argument
order); `cargo test -p p1-host --test sandbox` (end to end incl. a worker's shell). The mount
plan was first verified by hand by the lead (bubblewrap 0.9.0).
