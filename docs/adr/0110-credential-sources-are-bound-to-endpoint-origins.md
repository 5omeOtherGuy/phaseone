---
adr: 110
title: Credential sources are bound to endpoint origins
status: proposed
date: 2026-10-05
deciders: lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/486]
---
# ADR-0110: Credential sources are bound to endpoint origins

## Context

Issue #486 confirms two gaps after #484: new route ids could redirect borrowed or environment credentials, and an absent installation-prefix route directory removed the shipped-origin check. Lead decided this design for migration batch B14.

## Decision

Compile the source tree's `routes/*.toml` id, endpoint origin and credential kind table into p1; malformed shipped TOML fails the build. A shipped id keeps `check_shipped_origin`, independently of installed files.

Borrowed OAuth kinds and `borrow` lists may reach only compiled shipped origins of the same credential kind, regardless of route id. New API-key ids (environment or store) and user store-only OAuth ids require an origin approved in p1's protected credential store. `p1 login <id>` records the origin with the key; `p1 login <id> --trust-endpoint` approves an environment-keyed API route without reading or storing a key. Claude Code import records the origin with its store entry. Missing or mismatched approval refuses before each credential access or refresh and names the login command. Construction remains lazy: inspection such as `p1 env show` may assemble a custom route without using its credential.

Loopback endpoints (`127.0.0.1`, `::1`, `localhost`) need no origin record; the shipped-id rule still applies. `kind = "none"` sends no credential and is exempt.

## Consequences

Origin metadata resides in `auth.json.origins` beside `auth.json`, in the same protected credential-file family, using the checked private directory and staged 0600 writer. The origin check opens only metadata, never the credential document. Key/import writes share the store lock, revoke old approval first, publish the credential, then approve its origin: an interrupted login fails closed. Logout revokes approval, including an environment-only approval.

Existing shipped routes keep working without a migration of store entries. Custom routes need an explicit login/trust action. Borrowed sources cannot be redirected to arbitrary remote proxies by trusting their endpoint; use a store-only route with explicit approval instead. The host build adds only the workspace's existing TOML parser as a build dependency.

## Alternatives considered

Installation-prefix files as trust anchor were rejected because a standalone binary lacks them. Binding only shipped ids was insufficient because credentials could be referenced by a new id. Automatically trusting first use would let an untrusted route approve itself.

## Evidence

`crates/p1-host/tests/credential_origins.rs` covers same-id and new-id attacker endpoints, missing installation directories, malformed build inputs, trusted custom ids over scripted transport, no key lookup on refusal, protected metadata, loopback spellings, login and logout. The attacker/new-id and missing-prefix cases failed before the fix. Run `cargo test -p p1-host --test credential_origins` through the migration run's build slot.
