---
adr: 182
title: Client MCP servers of an ACP session join its generation and p1 acp advertises streamable HTTP
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: [695, 670, 792, 766]
---
# ADR-0182: Client MCP servers of an ACP session join its generation and p1 acp advertises streamable HTTP

## Context

An ACP client names MCP servers for each session in the `mcpServers` of `session/new`, `session/load`, `session/resume` and `session/fork`. Until now `p1 acp` accepted and ignored them (#695), below the dsh floor (owner 2026-10-10: dsh advertises `mcpCapabilities {http: true}` and uses the client's servers). ADR-0181 gave p1 an MCP client and named per-ACP-session servers as #695. Two facts shape the slice. A session process assembles its agent before it reads its input, so the servers would arrive after the generation was built; and a session that fails to start ended its process, which the client saw only as "the session has ended", without the reason ADR-0181 requires (an error naming the server). Stdio servers are the baseline every ACP agent takes; `mcpCapabilities.http` and `.sse` advertise the others, and `p1-mcp-client` speaks streamable HTTP, not legacy SSE.

## Decision

1. **Port.** `FrontEndPort` gains two default methods: `session_servers`, the servers the front end's client names for the session (`Vec<p1_contracts::mcp::ClientServer>`, a name plus a `Transport` holding the client's values only), awaited once before anything is assembled; and `start_failed(reason)`, called instead of `run` when the session cannot start, so a waiting client hears why. Both default to nothing, so the line front end, the TUI and every other port are unchanged. The host's `FrontEnd` seam forwards both.
2. **ACP adapter.** `AcpFrontEnd` starts its transport on first use rather than in `run`: `initialize` is answered at once, the first opened session's `mcpServers` go to the host, and the `session/new` answer waits for the agent as before. `wire::v1::mcp` decodes stdio servers (no `type`, or `"stdio"`) and `"type": "http"`; `"sse"`, an unknown type or a malformed entry is invalid params naming the entry, never a value. `start_failed` answers the waiting session request with `-32603` and the reason, then closes the connection. A load or resume that re-attaches to the open session keeps the servers it opened with.
3. **Host.** `crate::mcp::attach` connects the environment's servers, then the client's, into one generation. That is the one connection path, with ADR-0181's rules: the same name rule, a name the environment or another client server already uses fails the start, a tool-name clash fails the start, every name and transport is checked before the first server starts. A client stdio child gets the non-secret baseline (`ENV_ALLOW`, `ENV_ALLOW_PREFIXES`) plus exactly the variables the client lists, in the session's folder; nothing else is inherited. A client HTTP server gets the client's headers, none of the ones the transport sets itself. Every value the client sends is registered as a secret (and the credential of a `<scheme> <credential>` header value on its own), so it is masked in results, the journal, stderr and the wire; the journal names servers and tools only. Any start failure is masked, passed to `start_failed` and returned masked (the operator's stderr).
4. **Wire.** `initialize` advertises `agentCapabilities.mcpCapabilities = {"http": true, "sse": false}`, from the router and from a session process alike.

## Consequences

An editor's MCP servers serve tools to that session's main agent under the same approval, masking and journal as an environment's servers, and a session that cannot connect one fails `session/new` with an error naming it (fail closed). Every recorded `initialize` answer gains `mcpCapabilities`; the fixtures were re-recorded for that field only. A start failure of any kind, not only MCP, now reaches the client as the `session/new` error. Over-masking is possible, by the rule that every client value may be a credential: a non-secret value the client sends that is at least `MIN_SECRET_LEN` long (such as `production` or a folder path) is masked wherever it appears, and a served tool whose declaration holds such a value is refused as carrying a credential, which fails the start. Workers still get no MCP tools, a re-attach cannot change the session's servers, and legacy SSE servers are refused until the client adapter speaks SSE.

## Alternatives considered

Passing the servers from the router to the session process on its command line or in its environment: values would be visible in the process table or inherited by every child. A private preamble line on the session process's input: a second framing on a JSON-RPC pipe. Connecting the servers after assembly and swapping them in through `Agent::reconfigure`: a second connection path and an extra `Environment` record for every session. Applying the environment file's credential refusal (no literal tokens, no `Authorization` header) to client servers: a client has no other way to pass a token, and it is the operator's own front end.

## Evidence

`cargo test -p p1-host --test acp_mcp_servers` (a bash stdio server and a streamable HTTP server on 127.0.0.1 named in `session/new`: both tools declared and called, the child gets the baseline and the client's variables only in the session's folder, the bearer header reaches the server, no client value in results, journal, stderr or wire; a negative control without the secret registration fails it; a name the environment uses fails `session/new` naming `sh`; `sse` is `-32602` and a later `session/new` opens); `cargo test -p p1-acp --test mcp_servers` (servers reach the host before the answer, `start_failed` answers with the reason, a client that leaves names none); `cargo test -p p1-acp --lib wire::v1::mcp`; `cargo test -p p1-host --lib mcp::`; `cargo test -p p1-host --test acp_fixtures` and `--test acp_agent_state` (fixtures re-recorded, diff is `mcpCapabilities` only).
