---
adr: 78
title: Connection resources and component replacement
status: accepted
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

Every S5 slice that delivers this decision, with its merge commit on main and the main `gate` run of
that commit:

- S5.1, PR #227, merge commit `f06fe427` (`f06fe427dec17c998a84d91b79b9db8b63be5fa6`): this ADR,
  drafted and merged `proposed`; main gate run 36177477102, success.
- S5.5, PR #294, merge commit `7fe39b04` (`7fe39b04d367e58651fd23727f739b6c952feb4c`): the WebSocket
  resource of §1–§3 — the native host session (`crates/p1-provider-http/src/ws_session.rs`: TLS, the
  credential handshake, ping/pong and the ADR-0069 bounds on raw frames), the portable lower decision
  in `crates/p1-provider-openai/src/websocket_lower.rs`, and the boundary suite
  `crates/p1-module-tests/tests/websocket_boundary.rs`; main gate run 36228482257, success.
- S5.2, PR #293, merge commit `a1ba331e` (`a1ba331e4112f33220ff63b8216622a614c24990`): the context
  policy as the package `p1/context/summarizing`, with the runtime adapter that loads it, one of the
  components §4 replaces behind the boundary; its own main gate run 36224885320 was cancelled by the
  concurrency group and is covered by the descendant `9e82751d` run 36225249460, success.
- S5.3, PR #280, merge commit `7c9dae5e` (`7c9dae5e0f0a5d65f8c577cd13c64717afcf38b7`): the
  authorization-policy packages `p1/policy/full-access` and `p1/policy/ask`, the runtime adapter and
  the native ask bridge, the other component the host replaces by name; main gate run 36216644399,
  success.
- S5.7, PR #339, merge commit `806bc46e` (`806bc46ef107988957276a4c8522a315d4b8ad9d`): the atomic
  between-turns replacement of §4 — a model switch and `/modules reload` are one operation,
  committed and installed with no await between them, while runs, children and workflows keep the
  generation they were started with; main gate run 36257347985, success.
- S5.8, PR #337, merge commit `f893c97a` (`f893c97abbd7c571722d128f02a55e8f7d35e182`): replay across
  a provider switch (`crates/p1-module-tests/tests/replay_switch.rs`), the measured form of "the
  replacement opens its own connection, and its first turn sends the full input"; main gate run
  36251278327, success.
- S5.9, PR #350, merge commit `dd90cc4d` (`dd90cc4d645583b71e1b8c88c32e6b2288d40c14`): the handle
  lifetime of §3 and §4 across compaction, reload and reconnect
  (`crates/p1-module-tests/tests/compaction_workload.rs`), and the acceptance row `compaction-16`
  measured (`growth=none`) by the slice's case in `crates/p1-module-tests/tests/acceptance.rs`; main
  gate run 36257376533, success.
- Acceptance: this ADR is accepted with the recorded S5 verdicts — the Fable judge's ACCEPT on
  S5.1, S5.2, S5.3, S5.5, S5.7 and S5.8, and the lead's XO call on S5.9 (one review round plus a
  green gate, no judge).
