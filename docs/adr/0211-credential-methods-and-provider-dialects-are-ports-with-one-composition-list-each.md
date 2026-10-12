---
adr: 211
title: Credential methods and provider dialects are ports with one composition list each
status: accepted
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0040, ADR-0061, ADR-0074, ADR-0093, ADR-0153, ADR-0200, ADR-0209, ADR-0210, issue #886, issue #889]
---
# ADR-0211: Credential methods and provider dialects are ports with one composition list each

## Context

Owner, 2026-10-11, for wave 3 of the repository split (#886): "I want modules developed outside
this repository. The whole idea is that everybody can develop modules." ADR-0200 left the
subscription connectors' code in the core crates behind the cargo feature `subscriptions`. Its
reason: "Extracting that code into crates of its own needs a credential-method port and a
provider dialect port first".

Today `CredentialKind` is a closed enum, and code across crates matches on it:

- p1-auth: the table checks, the source chain, the store entry and its refresh;
- p1-host: login, logout, endpoint approval, the origin rules, the credential files the tools
  refuse;
- p1-usage: which probe a route answers.

Each provider crate keeps its subscription dialects as `cfg`-gated arms of a closed enum
(`MessagesAccount`, `ResponsesAccount`, the OpenCode client identity of the chat dialects). A
method or a dialect that is not in those enums cannot exist, wherever its code lives.

## Decision

- **A credential method is a port.** `p1_auth::CredentialMethod` is what a route's `[credential]`
  method means:
  - its name, label and owner text;
  - its own checks of the table;
  - what p1's store keeps for it (`StoreForm`: nothing, an API key, or OAuth tokens refreshed
    through an `OauthRefresh`);
  - the other tools' logins it borrows after the store (`BorrowedLogin`, each with its source
    name and its guidance);
  - whether it takes a `login_dir`;
  - the credential files a route of it reads, which the file tools refuse;
  - the usage probe its credential answers.

  `CredentialKind` becomes a handle on a method, parsed from the method's name. The chain (the
  documented variable, p1's store, the borrowed logins unless store-only), the "which source"
  report, the store and its OAuth rotation are written once over the port. The callers ask the
  method; none matches a list of kinds.
- **One composition list per port.** `p1_auth::methods()` is the one place a build's methods
  are listed. It returns `api-key` and `none` from the core, then the subscription connectors'
  `claude-code-oauth` and `codex-oauth`, built from `p1-auth/src/subscriptions.rs`, only when
  the build has `subscriptions`. Nothing registers itself, and a table naming a method the
  build lacks is refused with the list of methods the build has.
- **A provider dialect is a port of its provider crate**, built in the next change of #889. Each
  provider crate (`p1-provider-anthropic`, `p1-provider-openai`, `p1-provider-openai-chat`)
  turns its dialect enum into a dialect trait:
  - The trait carries what a dialect decides: the request shaping, the headers, the client
    identity, and which header carries the credential.
  - The crate has one list of its dialects: its own core dialects, plus the subscription ones
    when the build has them.
  - A route's `[adapter_settings] dialect` names an entry of that list.
  - The host's route-settings mirrors (ADR-0093) validate a dialect name through the crate's list
    instead of a copy of its spellings.
- **What follows in #889**: the module-side credential seam in the WIT, so that a provider module
  declares the method the host brokers for it. Then the move of the connectors' code out of
  the core crates into `connectors/subscriptions/`, which retires the feature. Each of these
  takes its own record when it opens.

## Consequences

- A credential method or a dialect is added by implementing a trait and adding it to one list,
  in the crate that owns the port. Nothing else in p1 changes for it.
- The store's OAuth entry and its rotation are core code now, independent of any provider: the
  token endpoint, the request encoding and the response a refresh reads are the method's
  `OauthRefresh`. The store's refresh error also names the transport's reason, as the Codex
  login's did.
- The connector-specific names that remain in the core's types are gone: `SourceName` names a
  borrowed login with the login's own name and guidance (`SourceName::Login`). The connectors'
  kinds and logins are constants of their module (`CLAUDE_CODE_OAUTH`, `CLAUDE_CODE_LOGIN`, …).
- Still connector code in the core crates, behind the feature, until the extraction:
  - the borrowed-login readers and the usage probes' shapes;
  - `p1 login --from-claude-code`;
  - the build-time check of the shipped method names.

## Alternatives considered

- **Keep the closed enum and add arms.** Every new method would edit four crates; this is what
  ADR-0200 named as the obstacle.
- **A registry the methods add themselves to.** Refused by ADR-0153: composition is explicit at
  one place.
- **Turning the OAuth logins into data now.** That means a token-endpoint descriptor and a
  borrowed-file layout, read by a generic broker. It is the likely end state for connectors that
  live in another repository (#893), but it rewrites the hardened login code (issue #484) in
  the same step as the seam. The port comes first, so that rewrite becomes one implementation
  among others.

## Evidence

- `crates/p1-auth/tests/methods.rs` covers four things:
  - a build composes `api-key` and `none`, plus the two OAuth methods with `subscriptions`;
  - a table naming an unknown method is refused with the list;
  - the chain, the report and the store-only rule run over a method defined in the test
    itself;
  - the guidance comes from the borrowed login.
- The existing suites of `p1-auth`, `p1-usage` and `p1-host` pass in both feature sets, with
  their kinds and source names renamed to the port's constants (see the pull request).
