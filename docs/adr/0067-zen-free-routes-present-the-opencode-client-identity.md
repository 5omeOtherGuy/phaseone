---
adr: 66
title: Zen free routes present the OpenCode client identity
status: proposed
date: 2026-09-24
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0066: Zen free routes present the OpenCode client identity

## Context

The OpenCode Zen free tier serves `mimo-v2.6-flash-free` and
`muse-spark-1.3-contributor-free` only to requests that look like OpenCode's own CLI.
A plain p1 Chat Completions request — p1's `user-agent` and the cache key as
`x-opencode-session` — is answered `403 FreeTierError` ("OpenCode's free tier can only be
used from within OpenCode") even with a valid Zen key; `space-bunny-free` is not gated.
The earlier feasibility spike concluded the gate was the request BODY (OpenCode's system
prompt plus its full tool set) from a real `opencode serve`, and recommended not adding a
route at all. Probes against the live endpoint (2026-09-24, recorded in
`docs/design/zen-client-identity-evidence.md`) show the gate is narrower and structural: it
accepts any system prompt, but requires OpenCode's CLI `user-agent`, an `x-opencode-session`
id in OpenCode's own `ses_<12 lowercase hex><14 alnum>` shape, and `bash` and `read` among
the declared tools. The owner decided on 2026-09-24 (~21:10) to implement the
"minimalcc-pi approach" in p1: p1's own requests present the OpenCode client identity so the
two models answer from p1 natively, while p1 stays the agent loop.

## Decision

Add a route-file setting `[adapter_settings] client_identity = "opencode"` to the
`openai-chat` adapter. It changes only the request's non-secret identity, never the message
encoding: it replaces p1's `user-agent` with OpenCode's, adds the `x-opencode-*` headers
with `ses_`/`msg_` ids derived from the route's cache key, and declares the tool names the
gate requires (`bash`, `read`) as function stubs when p1 does not already declare them. Only
the four shipped Zen free routes (`opencode-zen-1`, `-2`, `-3`, and the `opencode-zen-free`
alias) set it; every other route keeps p1's own identity. The setting is data, so the
adapter names no endpoint and no route is hard-coded.

## Consequences

- p1 reaches MiMo and Muse natively, with p1's own prompt, tools and loop; no OpenCode
  process is involved.
- p1 no longer claims in `routes.md` that it never presents a foreign client identity; the
  claim narrows to "except the OpenCode Zen free routes, which are gated on it."
- The injected `bash` declaration is visible to the model and is not dispatchable; a call to
  it is answered `Unavailable` by `p1-core` (exact-name dispatch), and its description points
  at `p1-tool-shell`'s `shell`. `read` is only injected when the environment does not grant
  its own `read`.
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
- **Injecting OpenCode's full `build` system prompt and 11 real tool schemas:** rejected as
  unnecessary and more deceptive; the gate accepts any prompt and empty-schema declarations.

## Evidence

- `docs/design/zen-client-identity-evidence.md`: every probe (element set → HTTP status,
  `error.type`), the minimal accepted set, the real-`opencode` relay capture, and the p1 live
  receipts.
- `crates/p1-provider-openai-chat/tests/client_identity.rs`: the exact wire shape with the
  setting on and the unchanged shape with it off.
- `crates/p1-host/tests/route_files.rs::the_zen_free_routes_present_the_opencode_client_identity`:
  exactly the four Zen routes carry the setting.
