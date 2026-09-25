---
adr: 78
title: Connection resources and component replacement
status: proposed
date: 2026-09-25
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [issue #222, epic #206, DECISIONS.md D22, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0047-the-codex-route-may-speak-websocket-an-adapter-local-transport-with-sse-as-the-fallback.md, docs/adr/0049-model-selection-and-switching-a-session-to-another-model.md, docs/adr/0069-provider-reads-are-bounded-inside-the-connection-first-byte-120-s-stream-idle-300-s.md, docs/adr/0033-a-session-resumes-only-on-the-route-and-model-that-recorded-it.md, docs/adr/0015-send-capable-boxed-future-contracts.md, docs/design/websocket.md, docs/design/modules/protocol.md]
---
# ADR-0078: Connection resources and component replacement

## Context

ADR-0071 (DECISIONS.md D22) moves every provider into a WebAssembly module the host loads by
name. WASI gives a module no sockets, so the WebSocket route of ADR-0047 can only exist as a host
function; ADR-0071 lists ADR-0047 among the ADRs the migration must revisit ("WebSocket through a
host function") but does not say where the connection lives, who owns its failure policy, or how
long it lives.

Three accepted or proposed decisions constrain the answer:

- ADR-0047 makes the WebSocket path adapter-local: `p1-provider-http` owns the connector seam and
  `p1-provider-openai` frames requests, feeds the existing parser, owns the connection's lifetime
  and keeps continuation state per live connection (`docs/design/websocket.md` §4–§6). Its failure
  rule has two halves: any failure before model-visible output falls back to the SSE request and
  turns WebSocket off for that provider instance; a failure after visible output is an ordinary
  `Transport` failure.
- ADR-0069 puts the read bounds (first frame, stream idle) in the connection's message loop, not in
  the adapter, because only the loop sees control frames: a timer around "give me the next text"
  cannot see a keep-alive ping and would call a live peer idle.
- ADR-0049 lets a running agent switch model with `Agent::reconfigure` between turns: the
  provider's own `validate` runs over the current history, and the new environment is committed
  before anything is sent. With modules, a module reload is a second way to replace the provider
  of a running agent, and a replaced component must not inherit a connection or a continuation
  from the one it replaces.

`docs/design/modules/protocol.md` already fixes that retry, backoff and the one refresh on 401 stay
in the native transport broker and that a module only classifies. ADR-0015 keeps one owner per
agent's mutable state. ADR-0033, as superseded by ADR-0049, keeps the transport out of a
response's origin, so a session may move between SSE and WebSocket freely.

## Decision

**1. Placement.** The WebSocket connection is a host resource. The provider component receives it
through the websocket capability, which `p1-provider-http` owns natively together with the
`p1-auth` credential broker. The capability is route-bound: it connects only to the endpoint the
route configures, and a component cannot name another. The native host service keeps:

- TLS and the handshake, including the credential and ADR-0047's handshake policy (the one forced
  refresh on 401/403, `RateLimited` on 429), because retry, backoff and the one refresh on 401 stay
  in the native transport broker;
- ping/pong, bounded writes (connect, send and the pong answering a ping) and raw-frame activity.

ADR-0069's first-frame and idle bounds therefore stay measured at the layer that sees every byte
and control frame. The component sees only text frames; it could not see a keep-alive ping, so a
bound placed in the component would repeat the defect ADR-0069 removed.

The component owns what is specific to the Responses wire: request framing
(`"type": "response.create"`), event classification through its parser, continuation decisions
(`previous_response_id` with only the new input items, under the rules of `websocket.md` §6) and
the decision to fall back to SSE.

**2. Pre-output and post-output.** ADR-0047's distinction is preserved exactly. A failure before
model-visible output of the request falls back to the SSE request and turns WebSocket off for that
provider instance. A failure after model-visible output is an ordinary `Transport` failure of that
response; the host never retries or replays it, and the host's turn-level retry
(`completion.md` §3b) applies as before.

The component is the side that knows model-visible output has been emitted: it classifies every
event and produces the stream events the core receives, so it alone can tell a text or tool-call
delta from a handshake, a created event or an error frame. The host service sees frames, not
meaning. Keeping the decision with the component keeps the rule exact: the host cannot fall back
or retry after output because it never makes that decision, and the component cannot hide a
post-output failure because every failure it reports after output is terminal for the response.
A host-side failure (a bound expiry, a closed socket, a write error) reaches the component as a
classified error, and the component applies the rule to it with the knowledge only it has.

**3. Lifetime.** A connection resource belongs to the one component instance (Store) that opened
it and to one assembly generation. Its handle never crosses Stores and never outlives its Store;
dropping the instance closes the connection. Continuation state is connection-local, as in
`websocket.md` §6, and is lost on reconnect, on context replacement (`ContextReplaced`) and on
component replacement; the next turn then sends the full input, as ADR-0047 already accepts.

**4. Component replacement.** Under ADR-0049 a model switch and a module reload are the same
between-turns operation:

- It happens only between complete turns, after outstanding tool calls settle.
- The complete candidate is built and validated first: the provider's `validate` over the current
  history, and its declared replay version.
- The candidate is then committed as one environment record and installed with no await between
  commit and installation, so no turn can observe a committed environment that is not installed.
- Any failure before the commit leaves the current assembly intact.
- Connection resources and continuation state never transfer to the replacement; the replacement
  opens its own connection, and its first turn sends the full input.
- Runs, children and workflows already running keep the generation they were started with, and its
  connections, until they end; new ones take the new generation.

This amends ADR-0047 (where the connection and its policy live) and ADR-0049 (what a switch
covers) for the module architecture. It supersedes nothing.

**5. Not decided here.** The WIT signatures and the capability allocation belong to S0's frozen
boundary (`wasm-boundary-v1`); the transport broker's internals and the split of
`p1-provider-http` belong to S4; the reload journal records and the approval keys belong to the
later "Reloadable policies" ADR.

## Consequences

- The WebSocket route keeps its behaviour across the migration: the same handshake policy, the
  same read bounds measured where ADR-0069 measures them, the same fallback and the same
  post-output rule. Only the process boundary between the halves moves.
- The credential never enters the component: the handshake that carries it is native, and the
  route binding keeps a component from opening a connection anywhere else.
- Two sides now carry parts of ADR-0047's policy (the broker for the handshake, the component for
  classification, continuation and fallback), and tests have to keep them in step, as ADR-0047
  already noted for `drive()` and the handshake.
- A handle tied to a Store and a generation can be held past its meaning. The risk sits where
  state changes under a live connection: compaction (context replacement), reload and reconnect.
  The stream's compaction-workload and reload suites cover it later; until they pass, the rule in
  §3 is stated, not proven.
- A replacement costs one full-input request on its first turn, and so does every reconnect and
  context replacement; no continuation saving survives any of them, as ADR-0047 already accepts.
- A running run, child or workflow can keep an old generation's connection open after a switch or
  reload, so for a time two generations may hold connections at once.
- S5's WebSocket slice implements this decision.

## Alternatives considered

- **Sockets in the component (a raw socket or TLS capability).** Rejected: the credential would
  enter the component, the handshake policy would leave the broker, and the component could not
  see control frames, so ADR-0069's bounds would have to move to where they are wrong.
- **The whole WebSocket transport in the host, the component receiving decoded events.**
  Rejected: framing, continuation and classification are Responses-specific, and the host would
  have to decide fallback without knowing whether model-visible output was emitted.
- **Keep the connection across a replacement when route and endpoint are unchanged.** Rejected: the
  continuation belongs to the previous component's requests and history shape, and a handle that
  crosses Stores breaks the one-owner rule; the cost of refusing is one full-input request.
- **Replace a component in the middle of a turn, or before outstanding tool calls settle.**
  Rejected: the history `validate` checks would still be changing, and ADR-0049 already limits a
  switch to between turns.

## Evidence

No measurement exists yet. This ADR is merged `proposed` before the WebSocket slice lands and is
accepted with Evidence in the PR that lands the phase's last DoD row.
