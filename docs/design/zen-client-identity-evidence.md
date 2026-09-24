# Zen free-tier client identity — probes and receipts

Owner decision 2026-09-24 (~21:10): p1 presents the OpenCode client identity to the Zen
free endpoint so `mimo-v2.6-flash-free` and `muse-spark-1.3-contributor-free` answer from
p1 natively. This file records what the gate actually accepts, the implementation's shape
(`ADR-0062`), and the live receipts. Probes were run 2026-09-24 against key 1
(`~/.config/keys/opencode-zen-1.key`); no key, token or header value with a credential is
recorded here. Requests were sent from a header file (`curl -H @file`) or a script that
reads the key file into memory, never on a command line.

## Method

`build_body.json` is a byte-for-byte capture of the request a real `opencode`
(1.18.31) `build` agent sent to a local mock. To confirm the capture is what Zen
accepts, a local HTTP relay received real opencode's request and forwarded it verbatim
over TLS to Zen (opencode's `baseURL` pointed at the relay). The relay's MiMo request
(body byte-identical to the capture) got **200**; its Muse request went to the
**Responses** path and got **200** (see "Muse" below).

Probes then varied one element at a time against the live endpoint. Status and
`error.type` are recorded; `200` means the response stream started.

## 1. The chat endpoint (`POST https://opencode.ai/zen/v1/chat/completions`)

### Headers

| # | headers (key 1) | body | result |
|---|---|---|---|
| 1 | p1's own (no opencode headers) | full opencode system prompt + 11 tools | 403 Cloudflare HTML |
| 2 | p1's own | neutral prompt + no tools | 403 Cloudflare HTML |
| 3 | `Accept, Accept-Encoding, Connection`, no opencode UA, no `x-opencode-*` | full body | 403 (Cloudflare) |
| 4 | opencode UA + short `ses_probe0001`/`msg_probe0001` | full body | 403 FreeTierError |
| 5 | opencode UA + valid `ses_…` + **short** `msg_…` | full body | 200 |
| 6 | opencode UA + **short** `ses_…` + valid `msg_…` | full body | 403 FreeTierError |
| 7 | opencode UA + valid `ses_…` only (no `x-opencode-client/project/request`) | full body | 200 |
| 8 | opencode UA + valid `msg_…`, no `ses_…` | full body | 403 FreeTierError |
| 9 | no opencode UA, valid `ses_…` | full body | 403 FreeTierError |
| 10 | no `Accept`/`Accept-Encoding`, opencode UA + valid `ses_…` | full body | 200 |

Only `User-Agent` and a valid `x-opencode-session` are required; `x-opencode-client`,
`x-opencode-project`, `x-opencode-request`, `Accept`, `Accept-Encoding` are free.

### `x-opencode-session` value shape

OpenCode's `Identifier.ascending` builds ids as `<prefix>_<12 lowercase hex><14 alnum>`.

| value | result |
|---|---|
| the real opencode id | 200 |
| fresh / now-2015 / all-zeros 12-hex prefix + random suffix | 200 |
| `ses_` + 26 random base62 (`lower_hex_prefix`) | 200 |
| `ses_` + `a`×26 | 200 |
| `ses_` + `0`×26 | 200 |
| `ses_` + `A`×26 (uppercase prefix) | 403 FreeTierError |
| `ses_` + `Z`×12 + random | 403 FreeTierError |
| `ses_` + 14 chars (too short) | 403 FreeTierError |
| 30 random chars (no `ses_` prefix) | 403 FreeTierError |
| `ses_` + `ABABABABABAB` + `CDCDCDCDCDCDCD` | 403 FreeTierError |

The gate accepts exactly `<prefix>_` + 12 **lowercase hex** + 14 alphanumeric. p1 derives
`ses_`/`msg_` ids from the route's cache key (a pure function, so a resume keeps it).

### System prompt and tools

All rows use opencode UA + a valid `ses_…`.

| prompt | tools | result |
|---|---|---|
| opencode's full 11060-char prompt | 11 real opencode tools | 200 |
| opencode's full prompt | 0 | 403 FreeTierError |
| neutral `P1RAW-SYSTEM-MARKER…` | 11 real tools | 200 |
| neutral | 0 | 403 FreeTierError |
| short "You are opencode…" identity | 11 real tools | 200 |
| opencode's full prompt | only `read` | 403 FreeTierError |
| neutral | 11 names with empty schemas | 200 |
| neutral | 11 real tools, descriptions dropped | 200 |
| neutral | 11 real tools + a 12th `p1_finish` | 200 |
| neutral | `bash` + `read` (empty schemas) | 200 |
| neutral | `bash` alone | 403 FreeTierError |
| neutral | `read` alone | 403 FreeTierError |
| neutral | `bash` + `edit`/`glob`/`grep`/`todowrite` | 403 FreeTierError |
| neutral | `bash` + `edit` + `glob` + `grep` (first 4) | 403 FreeTierError |
| neutral | first 5 (`bash,edit,glob,grep,read`) | 200 |
| neutral | `bash` renamed `Bash` | 403 FreeTierError |

The gate requires the request to declare BOTH a tool named `bash` and one named `read`;
the system prompt is free, the schemas are free, and extra tools are free. p1 keeps its
own tools and injects only the missing gate name (`bash`; `read` only when the environment
does not grant it) as an empty-schema function stub.

### Minimal accepted shape (chat)

- `User-Agent: opencode/…` plus `x-opencode-session: ses_<12 lower hex><14 alnum>`.
- Both `bash` and `read` among the declared tools (empty schemas suffice).
- Any system prompt; any stream setting; any max tokens.

## Muse is served on the Responses API, not Chat Completions

A real opencode `build` request for `muse-spark-1.3-contributor-free`, relayed verbatim
to Zen, went to **`POST https://opencode.ai/zen/v1/responses`** and got 200. Every
Chat-Completions request for muse — including the byte-exact opencode chat body and the
same identity headers — is answered:

```
503 {"error":{"type":"server_error","message":"Upstream request failed: Endpoint is unavailable."}}
```

i.e. the gate passed (no `FreeTierError`) but the chat endpoint has no muse upstream.
opencode's own metadata for the model is `@ai-sdk/openai` (Responses shape), which is why
opencode itself uses that path.

The Responses endpoint's gate is the same shape. Probes (key 1):

| headers | tools | result |
|---|---|---|
| opencode UA (1.18.32) + valid `ses_…` | 11 real opencode tools | 200 |
| opencode UA + valid `ses_…` | `bash` + `read` stubs | 200 |
| opencode UA + valid `ses_…` | none | 403 FreeTierError |
| no `ses_…` | `bash` + `read` | 403 FreeTierError |
| `ses_`+`A`×26 | `bash` + `read` | 403 FreeTierError |
| no opencode UA | `bash` + `read` | 403 Cloudflare HTML |

**Consequence.** p1's `openai-chat` adapter cannot reach muse, so the muse binding on
the chat Zen routes returns 503. Reaching muse natively needs a second route using the
`openai-responses` adapter pointed at `/zen/v1/responses`, an account behaviour that
sends the Zen key without the ChatGPT account id, the same `client_identity` mechanism,
and a profile with `thinking = "effort-level"` (the Responses adapter encodes only that
policy). That is deliberately NOT done in this slice: it changes the Codex adapter and is
a separate review. The chat identity above is what this slice lands.

## Live p1 receipts

Binary: `/mnt/build/cargo-target/zen-client-identity/debug/p1`, scratch config
`P1_CONFIG_DIR=/tmp/zenreceipt` (repo routes + the p1 store; a throwaway `environments/{mimo,muse,bunny}`
naming the Zen routes). Commands and output, verbatim:

```
$ P1_CONFIG_DIR=/tmp/zenreceipt p1 --env mimo --workspace /tmp/zenreceipt/ws "Reply with OK"
OK
model openai-chat/opencode-zen-1/mimo-v2.6-flash-free · in 2044 (cached 1984) · out 24 · cost unknown
model openai-chat/opencode-zen-1/mimo-v2.6-flash-free · in 2145 (cached 1984) · out 121 · cost unknown
→ finish {"status": "done", "summary": "…"}
← finish ok (1 lines)
Done.
total model openai-chat/opencode-zen-1/mimo-v2.6-flash-free · in 6464 (cached 6080) · out 152 · cost unknown
```
**MiMo: 200, token counts reported.**

```
$ P1_CONFIG_DIR=/tmp/zenreceipt p1 --env bunny --workspace /tmp/zenreceipt/ws "Reply with OK"
OK
model openai-chat/opencode-zen-free/space-bunny-free · in 2292 (cached 128) · out 11 · cost unknown
…
total model openai-chat/opencode-zen-free/space-bunny-free · in 4673 (cached 2429) · out 78 · cost unknown
```
**Bunny unchanged: 200, token counts reported** (it is ungated and keeps working with the
identity setting on).

```
$ P1_CONFIG_DIR=/tmp/zenreceipt p1 --env muse --workspace /tmp/zenreceipt/ws "Reply with OK"
! provider failed: Transport: chat HTTP status 503 (server_error)
… (4 attempts)
total model openai-chat/opencode-zen-1/muse-spark-1.3-contributor-free · in ? (cached ?) · out ? · cost unknown
```
**Muse: 503 from the chat endpoint** — the gate is passed, the endpoint has no upstream
(see above). Not a p1 client-identity failure.

## Companion fix: the free stream's repeated terminal chunk

MiMo's 200 stream ends with the stop chunk followed by a SECOND chunk that repeats
`finish_reason:"stop"` and carries the `usage` block (instead of `choices: []`). p1's
chat parser rejected any choice after a finish reason, so the first MiMo run retried
three times and reported `?` tokens. `parser.rs` now tolerates a post-stop choice whose
delta carries no content/reasoning/tool fragment; a genuine content choice after the stop
is still a protocol error. Bunny's stream does not need this (its usage chunk is
`choices: []`), so the relaxation is additive. Pinned by
`parser::tests::a_repeated_empty_terminator_with_usage_completes_instead_of_failing`.
