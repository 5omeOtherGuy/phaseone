---
adr: 104
title: Credentials are registered values masked everywhere, and credential files are written through a pinned directory
status: proposed
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: ["issue #484", "review4 findings (rv-auth/.review/findings.jsonl)"]
---
# ADR-0104: Credentials are registered values masked everywhere, and credential files are written through a pinned directory

## Context

The review of issue #484 (51 findings, 33 P1) showed two gaps in how p1 handles
credentials. First, masking knew only credential SHAPES (`sk-` keys, `Bearer`,
`Authorization`, a few JSON fields): an opaque OAuth token, a JWT or an API key without a
known prefix was persisted as-is when a tool printed it or a model echoed it, and model
output was not masked at all. Second, the three credential writers (p1's store, the Claude
Code login, the Codex login) wrote through path names: a symlinked or world-writable
directory, a planted staging file, a replaced lock file or a failed publish could redirect,
expose or lose a rotated login.

AGENTS.md and ADR-0103 forbid a process-wide registry, so the set of known credential
values has to be an explicit handle.

## Decision

1. `p1_redact::SecretSet` is an explicit, cloneable handle of the credential values p1
   resolved. The host owns one per `HostDeps`; every credential source the catalog builds
   is wrapped so each value it hands out is registered before an adapter sees it. Every
   `MaskCounter` the host composes carries the set, so tool output masks registered values
   as well as shapes; every provider the catalog builds is wrapped in a masking decorator
   (`p1-host/src/secret_mask.rs`) that masks registered values — never shapes — in text,
   reasoning and tool-input deltas (holding back a possible value prefix across deltas), in
   the committed item and in failure messages, leaving `ReplayData` intact. Tool names,
   schemas, grammars and identities that carry a credential are refused at assembly
   (`check_declaration`), never masked.
2. `p1-auth` reads and writes every credential file through a `CredentialDir`: a directory
   handle opened once and checked (owner, no write access for others along the spelled and
   the canonical path, sticky or private-group exceptions), files opened relative to it with
   `O_NOFOLLOW`, `O_NONBLOCK` and a size cap, a fresh exclusive staging file per write,
   `fsync` of file and directory, a directory `flock` held for the whole refresh, and a
   recovery file that keeps a rotation whose publish failed.
3. A route that takes the id of a shipped route may only send that route's credential to
   the shipped endpoint origin (`routes::check_shipped_origin`).

## Consequences

- A credential p1 used is masked wherever it later appears, without a shape. A value p1
  never resolved in this process (another route's variable, a refresh token) is still only
  caught by shape; the auth JSON field rule covers the credential files themselves.
- Model text that quotes a registered value shows a `<redacted:secret:N chars>` marker;
  ordinary text is untouched because no shape rule runs on model output.
- `p1-auth` depends on `rustix` (already in the workspace). Credential directories must be
  owned by the user and not writable by others; a shared-group directory is accepted only
  for the user's own primary group.
- A route override under a new id can still name a borrowed login or a variable and send it
  anywhere; that residual is tracked as a follow-up issue.

## Alternatives considered

- A global secret registry: rejected by AGENTS.md and ADR-0103.
- Shape masking of model output: rejected, it corrupts tool arguments and ordinary text.
- Binding every credential kind to a fixed origin: deferred; test and proxy routes use
  loopback and custom endpoints, and the id-keyed store makes the shipped-id override the
  case that exposes stored credentials.

## Evidence

Tests: `crates/p1-auth/tests/{credential_files,rotation,validation,process_contention}.rs`,
`crates/p1-auth/src/credential_file.rs` unit tests, `crates/p1-redact/src/lib.rs` tests
(`registered_values_are_masked_anywhere`, `mask_replaces_registered_values_and_no_shape`),
`crates/p1-host/src/secret_mask.rs`, `crates/p1-host/src/auth.rs` and
`routes::tests::a_shipped_route_id_cannot_send_its_credential_to_another_origin`. CI runs
them on the PR for issue #484.
