---
adr: 138
title: OpenCode Go DeepSeek environments default to Messages
status: accepted
date: 2026-10-09
deciders: owner
supersedes: []
superseded_by: []
sources: [environments/deepseek/environment.toml, environments/deepseek1/environment.toml, environments/deepseek2/environment.toml, environments/deepseek3/environment.toml, environments/deepseek-review/environment.toml, environments/deepseek-messages/environment.toml, routes/opencode-go-messages.toml]
---
# ADR-0138: OpenCode Go DeepSeek environments default to Messages

## Context

ADR-0134 (#623) added the Anthropic Messages wire for OpenCode Go as four extra routes
(`opencode-go-messages{,-1,-2,-3}`) and one trial environment, `deepseek-messages`, and
switched no existing environment: the switch was left to the owner after a measurement.
Issue #622 measured both wires on the same Go account and release. On 2026-10-09 the owner
decided (D38, issue #633) that the OpenCode Go DeepSeek environments use the Messages wire
by default.

## Decision

The five OpenCode Go DeepSeek environments name the Messages route of the same account:

| environment | before (Chat) | after (Messages) |
|---|---|---|
| `deepseek` | `opencode-go-subscription` | `opencode-go-messages` |
| `deepseek1` | `opencode-go-1-subscription` | `opencode-go-messages-1` |
| `deepseek2` | `opencode-go-2-subscription` | `opencode-go-messages-2` |
| `deepseek3` | `opencode-go-3-subscription` | `opencode-go-messages-3` |
| `deepseek-review` | `opencode-go-subscription` | `opencode-go-messages` |

Each Messages route reads the store entry of its Chat route (`credential_route`, ADR-0134),
so no key moves. The Messages routes already carry `retry_policy = "deepseek"` (ADR-0137)
and the `x-api-key` credential placement (ADR-0134; Bearer alone was refused with HTTP 401).
Nothing else in the environments changes: profile, options, tools, context and prompts stay.

The ClinePass environments `cline` and `cline2` stay on Chat: ClinePass has no Messages
endpoint (#622 probe M5, as stated in #633).

The Chat routes `opencode-go*-subscription` stay shipped, for other users and for rollback.
`deepseek-messages` stays as an alias of `deepseek`: removing it would delete a shipped
prompt file and edit inventory tests in four files (the model list, the row count of
`p1 models`, the prompt-coherence environment list, the alias prompt and TOML equality
check), while keeping it edits none, and a session or brief that names it keeps working.

`p1 usage` lists routes, not environments, and labels the Go accounts by their Chat route
ids (`opencode go`, `opencode go-1`, …), whose rows probe the Go usage endpoint with the
shared credential. The switch changes none of these rows, so each Go account is still
attributed once under its own label; a unit test pins this.

Rollback: point the five environments back at their Chat routes; nothing else changes.

## Consequences

- Known telemetry difference: Messages usage reports no separate reasoning-token count.
  The journal's `reasoning_output` is 0 on these environments although thinking blocks are
  returned; their output tokens include the thinking. Compare reasoning totals across the
  switch with that in mind.
- Thinking text and signatures are replayed under the Messages origin. A session started
  on a Chat route and resumed on its environment after this change keeps its history but
  drops the foreign-origin reasoning, as for any route change (ADR-0018).
- Exact shipped-inventory tests that pin these environments' routes now name the Messages
  routes; the behaviour tests of the Chat routes keep testing the Chat routes.

## Alternatives considered

- Keep Chat as the default and leave `deepseek-messages` as the opt-in: the measurement
  below favoured Messages and the owner chose the switch.
- Remove `deepseek-messages`: more inventory edits and a deleted prompt for no runtime
  gain (above). It can still go in a later clean-up once nothing names it.
- Remove the Chat routes: rejected by the issue; they are the rollback and serve other users.

## Evidence

- #622 before/after (n=1, 2026-10-09), same Go account and release: W0 review on Messages
  685 s / 57 requests, on Chat 958 s / 82 requests; an implementation fix 24 s vs 32 s;
  both wires correct in both tasks. Figures as recorded in #622 and quoted in #633; not
  re-measured for this record.
- `p1 models deepseek` lists the `opencode-go-messages*` routes for the five environments;
  `p1 models cline` still lists `cline-pass-1` and `cline-pass-2`.
- Tests: `crates/p1-host/tests/models.rs`, `crates/p1-host/tests/route_files.rs`, the
  `p1 usage` attribution test in `crates/p1-host/src/usage.rs`.
