---
adr: 46
title: An exhausted account is its own provider error kind; it is never refreshed or retried
status: proposed
date: 2026-09-21
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/design/providers.md, docs/design/research-program.md]
---
# ADR-0046: An exhausted account is its own provider error kind; it is never refreshed or retried

## Context

When the primary OpenCode Go account ran out of credit (2026-09-20), p1 answered "key rejected":
the chat adapter turned the 401 into a bare `Authentication` error, the shared driver refreshed
the credential, the refresh could only fail, and the driver surfaced the REFRESH error instead of
the server's diagnosis. The operator looked for a broken key for an account that simply had no
balance (issue #6). Research #35 traced the three drop sites and found the chat adapter to be the
outlier: the Anthropic and Responses adapters already surface a sanitised error type.

## Decision

`ProviderErrorKind` gains `InsufficientBalance`. An adapter may emit it only when the error body
names, in a fixed position, a word from a fixed allow-list; the server's text is used as a lookup
key only and the message is a constant. The shared HTTP driver finishes at once on this kind — no
credential refresh, no adapter retry — and the host does not retry the turn.

## Consequences

- The operator reads what is wrong and p1 stops spending requests on an account that cannot answer.
- `p1-contracts` changes: a journal that records the new kind cannot be read by an older binary.
  Both places that match on the kind have catch-all arms, so nothing else must change.
- The allow-list is a guess about wire shapes nobody could call live; an unknown shape falls back
  to today's behaviour, which is safe and merely unhelpful. The list grows on evidence.

## Alternatives considered

- A message-only fix in the chat adapter and the driver: no interface change, but the pointless
  refresh stays and the host cannot tell this failure from a bad key.
- A new field on `ProviderError`: 34 files for the same information.

## Evidence

`../phaseone-briefs/research/35/memo.md` (code paths re-opened by the curator; 241 uses of the
kind in 44 files, two matching sites, both with catch-alls). The live body shape is unknown.
