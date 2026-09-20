---
adr: 39
title: A provider is composed from a wire adapter, a route and a model profile
status: accepted
date: 2026-09-20
deciders: owner
supersedes: []
superseded_by: []
sources: [docs/design/notes/2026-09-20-provider-split.md, docs/design/providers.md, docs/design/pillars.md, crates/p1-contracts/src/provider.rs, crates/p1-assembly/src/lib.rs, crates/p1-host/src/catalog.rs]
---
# ADR-0039: A provider is composed from a wire adapter, a route and a model profile

## Context

p1 treated "provider" as one module that also carried model-specific wire behaviour. The
owner, 2026-09-20: "having providers as modules that are optimized for the models they provide
only works well if the provider is the creator of the models. With a opencode-go subscription
that provides access to many models, or openrouter, we will be running into issues with my
intended design." One account and one protocol then serve many model families, and the same
family reached over two routes had no guarantee of behaving the same. Asked whether to split
the provider into independent pieces the owner answered: "Yes absolutely that is exactly the
level of modularity we want." The design was worked out in a read-only consultation
(`docs/design/notes/2026-09-20-provider-split.md`), adopted here.

## Decision

Three independent inputs are composed into ONE runtime `Provider` (the existing seam stays):
- **Wire adapter** — few, compiled: Anthropic Messages, OpenAI Responses, Chat Completions.
  Encodes a model's policy into its protocol; owns stream parsing, usage decoding and the
  opaque replay codecs. A Chat adapter has a finite set of implemented DIALECTS, never a list
  of vendors.
- **Route/account** — data in the host's configuration layer: stable id, adapter key,
  endpoint, a credential REFERENCE (never a value), non-secret headers, per-model bindings
  (wire model name, limits, verified extensions). A compatible new endpoint needs no Rust.
- **Model profile** — plain data in a small crate `p1-model-profile` (depends on contracts
  only): identity, efforts, continuation requirements, limits, cache policy, accepted tool
  forms, plus a small enum of compiled behaviour strategies that selects SEMANTICS, never a
  route. Many environments reference one profile.
An environment names `route` and `profile`; its whole prompt file and exact tool list stay
where they are. Assembly resolves profile × route × binding × adapter and validates with the
same lowering logic the request builder uses; a combination that cannot be expressed fails
assembly. "The same model over two routes" means the same policy wherever it is expressible
and an assembly error otherwise — not identical JSON or portable reasoning data. `Origin`
stays route + configured wire model; cross-route resume stays rejected (ADR-0033).
Compile-time composition stays: configuration supplies constructor arguments; only adapters
wired into the host can run. No route trait, profile trait, generic protocol engine or plugin
loader.

Order (owner: "reshape first"): the unmerged `p1-provider-openai-chat` work is reshaped to
injected route data and separately supplied model behaviour BEFORE it merges; then profile
selection, data-driven routes and assembly validation; then the first-party adapters give up
their model policy step by step; the credential store is its own change (ADR-0040).
Each step passes the gate and the exact-commit CI check on its own.

## Consequences

- A new aggregator account is a settings entry; a new model is a profile; neither is a crate.
- The first-party adapters are NOT turned into a universal engine; they keep their request and
  parser code and lose only what is model policy, when a step needs it.
- Two defects found on the way are fixed within this work: the host's cache-key fallback retries
  assembly after ANY error (it becomes an explicit cache-key policy of the resolved route), and
  the Anthropic adapter silently raises an explicit output cap (it will reject the conflict).
- Several days of bounded work; shipped environment files change shape once (step 3), with
  explicit compatibility mappings for the shipped ones and no guessing for others.

## Alternatives considered

- Two axes (profiles inside adapters): duplicates model decisions per route.
- All profile fields in each environment: every prompt/tool variant repeats the model's
  continuation and effort rules.
- Traits for routes and profiles / a generic wire-protocol abstraction: not needed by any real
  model yet; revisit when the small strategies cannot express one, or when adapters duplicate
  substantial lowering logic.

## Evidence

The consultation note (files and functions cited there); the unmerged chat adapter's closed
`SubscriptionRoute` enum with ~18 match sites as the concrete instance of the coupling.
