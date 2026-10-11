---
adr: 186
title: An optional MCP server that fails to start is skipped with a notice and a record
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [802, 792, 695, 799, 766]
---
# ADR-0186: An optional MCP server that fails to start is skipped with a notice and a record

## Context

ADR-0181 and ADR-0182 fail the session start when any MCP server fails to connect: one flaky or uninstalled server makes every session of that environment unusable, resume included, and for ACP one broken server in the editor's configuration blocks p1 entirely. The owner ordered the fix on 2026-10-11 (#802): an environment can mark a server optional, a client's servers are optional, and a skipped server is never silent. ADR-0184 already gives masking a display-only notice path (`MaskNoticeSink` → `AgentEvent::ProviderNotice`) and the session's journal a place for a host-owned fact beside the core's records.

## Decision

1. **`required`.** A `[[mcp_servers]]` entry gains `required` (a boolean, default `true`); `McpServerSpec.required` carries it. A required server keeps ADR-0181's rule: a connect or listing failure fails the session start, naming the server.
2. **Client servers are optional.** A server from an ACP client's `mcpServers` cannot be marked and is always optional, so one broken editor entry does not block every session.
3. **Skip.** At session start the host connects each server (one retry, as before) and lists its tools on its own, bounded by the listing timeout (now per server) and the run's cancellation. An optional server that fails either step is left out: the session starts without its tools. Everything else still fails closed whatever the server: an invalid or duplicate server name, a tool-name clash, a tool name no route can carry, a declaration carrying a credential, and a cancelled start. `ToolGenerations::with_tools` builds generation 1 from the tools the host listed; `discover` and `refresh` list every server themselves, as before.
4. **Never silent.** Each skipped server yields one notice, `MCP server `<name>` skipped (optional), the session runs without its tools: <reason>`, emitted as an `AgentEvent::ProviderNotice` on the agent's event sink right after the first turn starts (`mcp::skip_notices`, outside `MaskNoticeSink`): the line front end prints it on stderr, the ACP adapter in the session process's operator log (ADR-0048, ADR-0184). It waits for the first turn because a front end may show nothing before its session is open: an ACP session process drops an operator notice until it has answered `session/new`. The reason is masked with the values that server's tools mask (the session's set, plus the client's values for a client server, ADR-0184), so it holds no credential and no value registered for masking; an environment server's literal `env` and `headers`, refused at load when they look like a credential, are not masked.
5. **Recorded.** The session's assembly identity line gains `mcp_skipped` (`p1_journal::SkippedMcpServer { name, reason }`, the same masked reason), absent when nothing was skipped, written by the start and kept by every later line of that session (a model switch or a reload).

## Consequences

An environment can list servers that are not always there without making its sessions fragile, and an editor's broken MCP entry no longer blocks p1: the session runs with the servers that answered and says which did not, and why. An environment server stays required unless it says otherwise, so a session that depends on one still fails closed. A skipped server does not come back during the session: nothing reconnects it, and a refresh lists only the servers the start connected; reconnecting and adding a skipped server's tools by a generation swap are a later slice. An ACP client sees the notice only in the operator log, as it sees the masking notice, and every front end sees it when the first turn starts rather than when the session opens: an interactive session that has not been prompted yet has not shown it, though its record is already written. Starting with several dead optional servers is slower: each costs its connect attempts and listing bound in turn. A client server's start failure now never fails `session/new`, so ADR-0184's masking of a start failure with every client value guards only the failures that remain (names, clashes, declarations). An assembly line with `mcp_skipped` is refused by an older p1 (the identity denies unknown fields), as the `instructions` and `skills` fields already are; a session that skipped nothing writes the same line as before. A server that answers slowly can now hold the start for its own listing timeout rather than one shared bound.

## Alternatives considered

Every server optional, environment servers included: an environment that needs a server would silently run without it. Skipping a server whose tools clash or cannot be named: ADR-0181 refused it, and a clash is a configuration error, not a flaky server. A new journal record for the skip: the core owns `RecordBody` and its sequencing. An inbox item: it would become model-visible history. Listing every server inside `ToolGenerations::discover` and dropping the failed optional ones there: the policy (which server is optional, its timeout, its masking) belongs to the host; the adapter only builds from what it is given.

## Evidence

`cargo test -p p1-host --lib mcp::` (`an_optional_server_that_fails_is_skipped_with_its_masked_reason`: a refused and an unlistable optional server skipped, the required one served, the echoed credential masked; `a_required_server_that_cannot_list_its_tools_fails_the_start`; `a_client_server_that_fails_is_skipped_and_its_values_masked`; `a_client_servers_tool_name_clash_still_fails_the_start`; a cancelled start stops for an optional server too); `cargo test -p p1-host --test mcp_servers` (`an_optional_server_that_fails_is_skipped_with_a_notice_and_a_record`: a command that does not exist and a closed 127.0.0.1 port, both `required = false`, the session runs with the working server, a notice per skipped server on stderr, `mcp_skipped` on the assembly line, no token anywhere; `a_required_server_that_fails_fails_the_session_start`); `cargo test -p p1-host --test acp_mcp_servers` (`a_failing_client_server_is_skipped_and_its_reason_recorded_masked`: `session/new` and the prompt succeed, no tool from the server, the reason echoing a client value is masked in the record, on stderr and on the wire; a negative control that masks the reason with the session's set only fails it and the unit test; run with `--nocapture`, the session process's operator log shows `p1 acp: MCP server `sh` skipped (optional), … bad token <redacted:secret:23 chars>`, which an emission before the first turn lost); `cargo test -p p1-assembly mcp`; `cargo test -p p1-journal --test journal_version`; `cargo test -p p1-mcp-client`.
