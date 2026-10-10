---
adr: 154
title: p1 speaks the agent side of ACP: p1 acp as the single interactive door
status: accepted
date: 2026-10-10
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0154: p1 speaks the agent side of ACP: p1 acp as the single interactive door

## Context

Epic #670 records the owner's decisions of 2026-10-09 and 2026-10-10:

- **D1:** `p1 acp` is p1's single interactive door, and every interactive screen is an ACP client. Owner: "So p1_acp it is, with our own TUI / GUI having all features via ACP extensions."
- **D4:** a prompt holds while background work it started runs.
- **D5:** the p1-tui freeze.
- **D6 amended:** no third-party ACP crate. `agent-client-protocol-schema` switches `serde_json/preserve_order` on for the whole binary, so p1-acp owns versioned wire types.
- **D7:** ports and adapters. Front ends attach through the ACP-neutral port of ADR-0152, and p1-acp holds the translation and the stdio driver.

The epic wrote this record's decision text before D6 and D7 were amended. The text below is that decision with those two amendments applied.

## Decision

`p1 acp` is p1's only interactive door. p1 implements the agent side of ACP v1 over stdio as a wire adapter behind the front-end port of ADR-0152.

- **p1-acp** holds the mapping, its own versioned wire types, the newline JSON-RPC 2.0 transport and the session driver. p1-host only composes it: the `p1 acp` command, the branch point in `run_agent`, and stdout→stderr hygiene, so that stdout carries JSON-RPC only.
- **Isolation:** p1-core and p1-contracts never learn ACP. The line front end stays the headless driver.
- **Reference clients:** p1's own clients (a future TUI or GUI) are reference implementations of the published ACP surface, which is `docs/acp/p1-extensions.md` plus the fixtures under `docs/acp/fixtures/`. They use no capability a third party could not declare, and they are tested over stdio.
- **Extensions:**
  - Ecosystem-agreed shapes are preferred over p1-specific ones.
  - Each p1-specific extension lives under `_p1/...` and `_meta["p1.dev"]`, and is sent only to a client that declared `clientCapabilities._meta["p1.dev"]`.
  - Each one names its standard successor and is retired when that successor stabilises.
- **Background work:** `session/prompt` is held open while a workflow run or a delegated worker that the prompt started is live, and until the parent's resulting inbox turn ends. `session/cancel` or the next prompt releases the hold. A shell job never holds.
- **Scope:** one session per process. `session/load`/`resume`, Hydra-style attachment and browser transports are deferred. Multi-session is a later child issue of the epic (D8).
- p1-tui stays frozen as a parts donor.
- Additive extensions are recorded in `docs/acp/p1-extensions.md`, not in further decision records.

## Consequences

- Any ACP v1 client can drive p1 today. acpx and Martty were proven on a real task in #673, and the ACP TCK runs locally.
- Gaps the first slice leaves:
  - Questions take the headless path until #674.
  - Workers report on stderr until #681.
  - Every tool call is put to the client, `finish` included, until approvals follow `--ask`.
  - A tool's title is its name: the port does not carry the assembled tools.
- A protocol version is a codec in `p1-acp/src/wire/`. v2 is added beside v1 and negotiated at `initialize`.

## Alternatives considered

- p1's own public protocol with an ACP translator. Rejected in the epic: three parallel protocols is DeepSeek Harness's cost, not p1's size.
- The SDK crate `agent-client-protocol`, or the schema crate alone. Measured in #671, then rejected by D6 amended because of the `preserve_order` side door.
- A driver in p1-host (the epic's first layout). Replaced by D7: p1-host only composes.

## Evidence

- `cargo test -p p1-acp`: the driver over an in-memory pipe against a fake session handle, `tests/driver.rs`. It covers:
  - the handshake, and one session per process;
  - prompt streaming;
  - cancel denying a parked approval and calling both cancel hooks;
  - the hold, and its release by cancel and by the next prompt;
  - every line parsing as JSON-RPC;
  - nothing extra reaching a non-declaring client.
- `cargo test -p p1-host acp`: the `p1 acp` command line, and the fixture replay `docs/acp/fixtures/prompt-tool-approval.jsonl` against the real host on scripted providers.
- PR for #673: the acpx NDJSON transcript, the Martty capture and the TCK report.
