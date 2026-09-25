---
adr: 67
title: Zen free routes present the OpenCode client identity
status: proposed
date: 2026-09-24
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0067: Zen free routes present the OpenCode client identity

## Context

The OpenCode Zen free tier serves `mimo-v2.6-flash-free` only to requests that look like
OpenCode's own CLI. A plain p1 Chat Completions request — p1's `user-agent` and the cache
key as `x-opencode-session` — is answered `403 FreeTierError` ("OpenCode's free tier can
only be used from within OpenCode") even with a valid Zen key; `space-bunny-free` is not
gated. `muse-spark-1.3-contributor-free` is gated the same way, but the Chat Completions
endpoint has no upstream for it: past the gate it answers `503`, so it is NOT shipped on
these routes — Muse stays on opencode (owner exception). The earlier feasibility spike
concluded the gate was the request BODY (OpenCode's system prompt plus its full tool set)
from a real `opencode serve`, and recommended not adding a route at all. Probes against the
live endpoint (2026-09-24, recorded in `docs/design/zen-client-identity-evidence.md`) show
the gate is narrower and structural: it accepts any system prompt, but requires OpenCode's
CLI `user-agent`, an `x-opencode-session` id in OpenCode's own
`ses_<12 lowercase hex><14 alnum>` shape, and both a `bash` and a `read` tool declaration.
The owner decided on 2026-09-24 (~21:10) to implement the "minimalcc-pi approach" in p1:
p1's own requests present the OpenCode client identity so MiMo answers from p1 natively,
while p1 stays the agent loop.

## Decision

Add a route-file setting `[adapter_settings] client_identity = "opencode"` to the
`openai-chat` adapter. It changes only the request's non-secret identity, never the message
encoding and never the tool set: it replaces p1's `user-agent` with OpenCode's and adds the
`x-opencode-*` headers with `ses_`/`msg_` ids derived from the route's cache key. The
adapter declares no tool of its own — a provider translates wire, it owns no tool surface —
so the gate's `bash` and `read` names are satisfied by the ENVIRONMENTS instead: the
shipped `zen`, `zen2` and `zen3` environments give the `shell` tool the face name `bash`
and the `read` tool its own name `read` (`[[tools]] name = "…"`). Only the four shipped Zen
free routes (`opencode-zen-1`, `-2`, `-3`, and the `opencode-zen-free` alias) set the
identity; every other route keeps p1's own identity. The setting is data, so the adapter
names no endpoint and no route is hard-coded.

## Consequences

- p1 reaches MiMo natively, with p1's own prompt, tools and loop; no OpenCode process is
  involved. Muse is deliberately not shipped on these routes (its chat endpoint answers 503).
- p1 no longer claims in `routes.md` that it never presents a foreign client identity; the
  claim narrows to "except the OpenCode Zen free routes, which are gated on it."
- The gate names are environment data, not an injected stub: in the Zen environments `shell`
  is presented to the model as `bash` and `read` as `read`, so a model call is dispatchable
  normally and no foreign, unavailable declaration exists.
- The session id is a pure function of the cache key, so provider-side prompt-cache affinity
  and a resumed session keep it; without a cache key a per-request nonce is used.
- If OpenCode changes its gate (user-agent version, id shape, required tools), the route
  starts failing `FreeTierError` again; the evidence file records how to re-probe.

## Alternatives considered

- **A real `opencode serve` worker backend** (the spike's recommendation): rejected for this
  purpose because it does not keep p1 as the agent loop. It remains valid for whole-task
  delegation and is out of scope here.
- **A `ClientIdentity` keyed by endpoint or route id in the host:** rejected — the host
  would name a vendor; route data is the mechanism the ADR-0039 split uses for exactly this.
- **An injected `bash`/`read` stub in the adapter:** rejected — providers own no tool; the
  environment's `[[tools]] name` is the mechanism that grants a model-facing name.
- **Injecting OpenCode's full `build` system prompt and 11 real tool schemas:** rejected as
  unnecessary and more deceptive; the gate accepts any prompt and empty-schema declarations.

## Evidence

- `docs/design/zen-client-identity-evidence.md`: every probe (element set → HTTP status,
  `error.type`), the minimal accepted set, the real-`opencode` relay capture, and the p1 live
  receipts.
- `crates/p1-provider-openai-chat/tests/client_identity.rs`: the exact wire shape with the
  setting on (headers only, no injected tools) and the unchanged shape with it off.
- `crates/p1-host/tests/route_files.rs::the_new_opencode_account_routes_are_distinct`:
  exactly the four Zen routes carry the setting, and each binds only the MiMo and Space
  Bunny profiles (Muse is not shipped).