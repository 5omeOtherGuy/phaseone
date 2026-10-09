# Provider modules — specification

Wire facts: `routes.md`. Contract: `crates/p1-contracts/src/provider.rs` (five stream rules
at the top of that file). A provider translates wire behaviour for one route; it executes
no tools and chooses neither prompt nor tool set.

## Crates

| Crate | Owns | Donor |
|---|---|---|
| `p1-provider-http` | `Transport` trait + the real reqwest transport, SSE decoder, HTTP status classification, retry policy/loop, a scripted transport for tests (feature `testing`) | `mimir/providers/transport.rs`, `mimir/retry.rs` |
| `p1-provider-conformance` | THE shared conformance suite: generic checks every adapter must pass, parameterised by route fixtures | — |
| `p1-provider-anthropic` | Claude subscription route: request build, SSE parse, replay, usage, OAuth credential reuse + refresh | `mimir/providers/anthropic_messages.rs`, `mimir/auth/anthropic.rs` |
| `p1-provider-openai` | Codex subscription route, likewise | `mimir/providers/openai_codex_responses.rs`, `mimir/auth/openai_codex.rs` |

Adapters depend on `p1-contracts` and `p1-provider-http`; never on the core, a tool or each other.

For the OpenAI Chat Completions stream, the finish choice is terminal except for one
OpenRouter-proxied gateway shape: ClinePass may repeat the same empty finish choice
(the delta has no content, reasoning, or tool calls) in the final usage chunk. The
repeat may carry usage, which is recorded as the latest usage report, but it does not
emit a second finish; every other choice after a finish remains a protocol error.

## `p1-provider-http`

```rust
pub struct HttpRequest { pub url: String, pub headers: Vec<(String, String)>, pub body: Vec<u8> }   // always POST
pub struct HttpResponse { pub status: u16, pub headers: Vec<(String, String)>, pub body: ByteStream }
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;
pub struct TransportError(pub String);                       // never contains header values
pub trait Transport: Send + Sync {
    fn post<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, TransportError>>;
}
pub struct ReqwestTransport;                                 // rustls, no default features, streaming body

pub struct SseEvent { pub event: Option<String>, pub data: String }
pub struct SseDecoder;                                       // push(&[u8]) -> Vec<SseEvent>; finish() -> Option<SseEvent>
```
The broker refuses an SSE frame above 1 MiB, including an unterminated line kept alive
by repeated chunks; multiple valid frames in one large transport chunk are accepted
while the chunk's decoded batch stays within the same 16 MiB bound — the decoder
refuses a larger batch before retaining it, and charges each frame's bytes to the frame
they belong to regardless of where the transport split the chunks (a pending bare-`\r`
separator is resolved before the next frame is charged). Each
adapter refuses a decoded event above 1 MiB, a response over
16 MiB, or more than 4096 output blocks/calls; these are Protocol failures.

SSE decoder rules (donor tests port over): events split by a blank line; `\n`, `\r\n` and
`\r` line endings; several `data:` lines join with `\n`; one optional space after the colon is
stripped; `:` comment lines ignored; a chunk may end anywhere — in the middle of a line, of a
multi-byte UTF-8 character, or between `\r` and `\n`; `finish()` flushes a final event that
lacks the trailing blank line. The host uses fallible `try_push`: one chunk/event is limited
 to 1 MiB and an unfinished line to 256 KiB; exceeding either ends the attempt as Protocol
 without replay. Events that completed before the violating line in the same chunk are still
 delivered first (`try_push_partial`), so a terminal event that precedes an over-limit tail
 still finishes the stream; `try_finish` applies the same cumulative event bound to the EOF
 flush. The bounds are public (`SSE_LINE_LIMIT`, `SSE_EVENT_LIMIT`, `SseLimitExceeded`).
 `Debug` shows lengths, never peer event names or data.

Status classification: `401|403` → `Reauth`; `408|425|429|500..=599` → `Retry`; any other
non-2xx → `Fatal`. `Retry-After` integer seconds honoured, clamped to 4 × the backoff cap.
The default remains `RetryPolicy { max_retries: 3, base: 2 s, cap: 60 s,
jitter: ≤ 250 ms }`, doubling. ADR-0137 adds route `retry_policy = "deepseek"`:
five retries, 500 ms doubling, 10 s cap after ±10% multiplicative jitter. A
Retry-After up to 10 s is exact; a longer hint surfaces the original failure
without a retry, not a shortened server-requested wait. Host composition gives
this policy to `WasmProvider`; it does not enter the component or WIT settings.

Retry loop invariants (each has a test, with a fake clock — `tokio::time::pause`, no real sleeps):
1. `Reauth` forces ONE credential refresh and one re-send per request; a second 401/403 is
   `ProviderErrorKind::Authentication`. It does not consume or reset the transient budget.
2. Transient failures (connect error, `Retry` status, stream broken BEFORE any content event
   was yielded) share one budget of `max_retries`.
3. **Never retry once any `TextDelta`/`ReasoningDelta`/`ToolInputDelta` has been yielded** —
   the failure becomes the terminal `Finished(Failed(Transport))`.
4. Back-off waits race cancellation. Before each wait the driver emits a `StreamEvent::Notice`
   (`provider returned HTTP <status>; retry <n>/<max> in <wait>`, or `provider request failed; …`
   when there is no HTTP status), then `StreamEvent::Activity`, so the consumer sees life.
5. Error messages carry status and fixed, enumerated diagnoses, never arbitrary peer code,
   response body or credential header value. The existing OpenAI native parser test explicitly
   requires a sanitized `x-request-id` value in HTTP errors. Both OpenAI and Anthropic expose
   only vendor-shaped request ids: `req_` followed by nonempty ASCII alphanumerics or underscores,
   at most 64 bytes total, without trimming. Arbitrary token-shaped headers are omitted.
   The frozen Anthropic `http_error_message_names_status_type_and_request_id_but_no_body`
   likewise requires a request id of this shape; omission of all peer request ids remains
   pending owner review.
   Frozen OpenAI `failed_and_error_events_map_the_code_and_hide_the_message` and chat
   `a_refused_request_names_a_short_code_and_never_free_text` /
   `an_unrecognised_body_keeps_todays_authentication_error` require specific additional
   token-shaped codes; these exact codes remain displayable.
   Component classification failures (Protocol) end without refresh or request replay.

Read bounds (issue #164). Every provider wait is bounded, so a request that never answers ends as
a named failure instead of hanging the agent. `FIRST_BYTE_TIMEOUT` = **120 s** is the wait for the
response headers (HTTP) or the first frame (WebSocket) after the request was sent;
`STREAM_IDLE_TIMEOUT` = **300 s** is the wait between two chunks/frames of an OPEN stream. ANY
received byte or frame — an SSE comment, or a WebSocket ping or pong — resets the idle clock, so a
keep-alive peer is never called idle. An expiry is a `ProviderErrorKind::Transport` failure whose
message names the bound (`no response within 120 s` / `stream idle for 300 s`); a first-byte expiry
follows the transient-retry policy above, while an idle expiry after any content event is terminal
(rule 3). While the first-byte wait is running the driver emits ONE `StreamEvent::Notice`
(`waiting for the provider (30 s)`), so the wait is visible to the operator.

`ScriptedTransport` (feature `testing`): a queue of canned responses — status, headers, and a
body given as a list of byte chunks, optionally ending in a transport error or hanging
forever — and a record of every `HttpRequest` it received.

## Adapter shape (both adapters)

```rust
pub struct <Route>Provider { /* transport: Arc<dyn Transport>, credentials: Arc<dyn CredentialSource>, model, retry */ }
pub trait CredentialSource: Send + Sync {                    // defined in p1-provider-http
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>>;
    /// Called after a 401/403 with the credential that was rejected; must not return it again.
    fn refresh<'a>(&'a self, rejected: &'a Credential) -> BoxFuture<'a, Result<Credential, ProviderError>>;
}
pub struct Credential { pub bearer: String, pub account_id: Option<String> }   // Debug prints "<redacted>"
```
- `build_request(&ProviderRequest) -> Result<serde_json::Value, ProviderError>` is a pure,
  public function, tested against golden JSON.
- The response parser is a pure state machine: `SseEvent`s in, `StreamEvent`s out.
- `validate` rejects: a `Freeform` declaration on a route without freeform tools; an explicit
  option the route cannot carry (`max_output_tokens` on the Codex route); unknown keys inside
  the adapter's own `native` namespace. Keys of other namespaces are ignored.
- Replay: `ReplayData{origin, version: 1, payload}` is emitted per reasoning block and sent
  back byte-exact ONLY when `origin` equals this provider's origin; foreign reasoning blocks
  are dropped from the request (their text is not converted into assistant text).
- **Origin is the CONFIGURED route + model**, for the item and for its replay data — never the
  model name a response echoes (providers answer with dated aliases; gating replay on the
  echoed name would drop every reasoning block on the next request and break tool use with
  thinking). Found by conformance check 8 on the first adapter.
- Usage mapping is in `routes.md`; absent fields stay `None`; `cost_micro_usd` is `None` on
  subscription routes. Chat rejects contradictory cache splits; Responses retains its frozen
  saturating subtraction for contradictory totals (owner decision pending). A completed tool call
  needs a nonempty unique identity and name;
  truncated or refused answers do not expose executable calls. Announced Responses output
  items must close with matching kind and identity; conflicting terminal response IDs fail.
  Malformed block/message framing is a protocol failure, not an empty successful answer.
  Anthropic pre-start content may emit display deltas on a truncated stream (frozen no-retry
  behavior), but cannot become a completed message even if message_start arrives later. A Responses cache key keeps the
  frozen Unicode/64-character clamp behavior; control characters and spaces are rejected
  before header assembly.

## Credentials (file-based sources live in the adapter crates)

Both adapters REUSE the owner's existing CLI logins; p1 has no login flow in this slice.
- Read the token file fresh on each `access` (another program may have rotated it).
- Refresh only when expired (5 min margin) or after a rejection; refresh tokens ROTATE, so
  the new token set is written back to the SAME file: take an advisory lock on a sibling
  `.lock` file, re-read (someone else may already have refreshed — then use that), refresh,
  write atomically (temp + rename, mode 0600), unlock.
- Unknown fields in the file are preserved verbatim on write-back.
- No token ever reaches a log, an error message, a `Debug` output, a fixture or the journal.

Tests use temp files with fake tokens and a `ScriptedTransport` for the refresh endpoint.

(Since ADR-0039 step 5 the sources themselves live in `p1-auth` — `docs/design/credentials.md`;
the rules above are unchanged.)

## An exhausted account (ADR-0046)

A 401/403 whose body says the account has no balance is NOT an authentication failure.
- `ProviderErrorKind::InsufficientBalance`, message exactly `the account has no balance`
  (a constant — the server's text is never copied, sliced or formatted into it).
- The chat adapter's `on_http_error` takes candidate words from `/error/type`, `/error/code`,
  top-level `/type` and `/code` of a JSON body (strings only), lower-cases them and looks them up
  in a fixed allow-list: `creditserror`, `insufficient_balance`, `insufficient_quota`,
  `quota_exceeded`, `billing_error`. A hit → the new kind. No hit, no JSON, empty body → exactly
  today's behaviour.
- The shared driver finishes immediately on this kind: no credential refresh, no retry, ONE
  HTTP request in total. The host's turn-level retry (completion.md §3b) does not cover it.
- The host prints the message as it prints any provider error; exit code as for any failed run.

## A plan that does not allow the model (ADR-0062)

A 401/403 whose body says the account's plan does not allow this model on this route is NOT an
authentication failure: the key is valid, so a credential refresh cannot help either. Observed
live on the OpenCode Zen chat endpoint, which answers a model gated to OpenCode's own client with
HTTP 403 and a `FreeTierError`-shaped body while the key is fine.
- `ProviderErrorKind::NotEntitled`, message exactly
  `the account's plan does not allow this model on this route` (a constant — the server's text is
  never copied, sliced or formatted into it).
- The chat adapter's `on_http_error` reads the same four positions and lookups as the no-balance
  check, with a second fixed allow-list: `freetiererror`, `not_entitled`, `plan_not_allowed`.
  A hit → the new kind. No hit, no JSON, empty body → exactly today's behaviour (a 401/403 stays
  `Authentication`).
- The shared driver finishes immediately on this kind, exactly as on `InsufficientBalance`: no
  credential refresh, no retry, ONE HTTP request in total.
- The Anthropic Messages and OpenAI Responses adapters classify 401/403 by status alone: neither
  route is bound to the Zen free tier, so neither can receive Zen's `FreeTierError` plan-refusal body.
- The TUI renders `✗ not included in the plan · <message> · not retried` and offers `/model`.

## A used-up usage allowance

A 402/429 whose body names a fixed usage-limit or quota word is not a short rate-limit window.
- The chat adapter reads the same four JSON positions as the no-balance and plan-refusal checks.
  The case-insensitive allow-list is `gousagelimiterror`, `insufficient_quota`, and
  `usage_limit_exceeded`.
- Z.ai string codes at `/error/code` also stop on 402/429: 1304, 1308–1311, 1313,
  and 1316–1321 (ADR-0140). Short limits 1302/1303/1305/1312 keep today's retries.
- Kimi refuses a used-up 5-hour/weekly window with a 403 whose type is
  `access_terminated_error` (captured live, #647; read in the same four positions,
  case-insensitively). That 403 stops the same way, with no credential refresh. Every
  other 403 keeps today's classification, including Kimi's concurrency 403, which has
  no documented type.
- A hit becomes `ProviderErrorKind::UsageLimitExhausted` with the fixed message
  `the account's usage allowance is used up`. An integer-seconds `Retry-After` (or rate-limit
  reset header) appends `(resets in <duration>)`; provider free text is never copied.
  Without that header, a Z.ai reset-at message's validated `next_flush_time` calendar
  timestamp is UTC+8 if zone-less, or UTC if marked `Z` (ADR-0144). A future reset
  appends `(resets in <duration>)`; past/equal times and unknown formats omit the hint.
  Response `Date` supplies the clock for both native and component parsing. Without
  a usable Date, native parsing uses system time; clockless components omit the hint.
- The shared driver finishes on the first response, with no refresh or retry. The host's
  turn-level retry does not cover this kind either. Unknown 402/429 bodies keep their existing
  status classification (`RateLimited` for 429) and retry budget.
- The TUI renders `✗ usage limit reached · <message> · not retried` and offers the reset or
  `/model`.

## The ONE conformance suite — `p1-provider-conformance`

```rust
pub struct RouteUnderTest {
    pub name: &'static str,
    /// Build a provider wired to this scripted transport, with a fixed fake credential.
    pub build: fn(ScriptedTransport) -> Arc<dyn Provider>,
    /// Declared retry budget; exhaustion asserts exactly this + 1 attempts (ADR-0137).
    pub max_retries: usize,
    pub fixtures: RouteFixtures,
}
/// Route-native SSE bodies (hand-written, real-shaped) for the SAME scenarios.
pub struct RouteFixtures {
    pub text_turn: &'static str,            // text "Hello" + " world", usage present
    pub tool_call_turn: &'static str,       // text, then ONE call: id "call_1", name "read", args {"path":"a.txt"}
    pub two_tool_calls: &'static str,       // "call_1" read, "call_2" grep — order must be preserved
    pub truncated_tool_call: &'static str,  // stream ends in the middle of a call's arguments
    pub invalid_tool_json: &'static str,    // a COMPLETE call whose arguments are not valid JSON: `{"path": `
    pub error_event: &'static str,          // provider-side error event mid-stream
    pub no_usage: &'static str,             // completes without any usage fields
    pub reasoning_turn: &'static str,       // reasoning with replay data, then text
    pub events_after_terminal: &'static str,// a complete turn followed by stray events
}
pub fn run_all(route: &RouteUnderTest);      // panics with the check name on the first violation
```
Checks (the enumeration is authoritative; each is one named function):
1. `text_deltas_then_single_terminal` — deltas in order; exactly one `Finished`; it is last; item text `Hello world`.
2. `tool_call_is_complete_and_only_in_terminal` — no `ToolCall` before `Finished`; id/name/raw args as given; `stop == ToolUse`.
3. `tool_call_order_is_preserved`.
4. `truncated_stream_is_failure_not_completion` — `Finished(Failed{Transport})`, no call surfaced anywhere.
5. `invalid_tool_json_is_preserved_raw` — the call IS surfaced with `ToolInput::Json` holding the raw invalid text (validation is the tool's job); never `{}`.
6. `error_event_is_single_failed_terminal` — and the message contains no response body text.
7. `unknown_usage_is_none_not_zero`.
8. `reasoning_replay_round_trips` — replay data from `reasoning_turn`, put into a follow-up request's history, appears byte-exact in the built request; with a foreign origin it is absent.
9. `nothing_after_terminal` — stream yields `None` after `Finished`; stray events are not surfaced.
10. `chunking_is_irrelevant` — every fixture split at EVERY byte offset into two chunks yields identical events.
11. `cancel_before_first_byte` and `cancel_mid_stream` (hanging body) — `Finished(Cancelled)` promptly, exactly once.
12. `http_401_refreshes_once_then_fails_authentication`; `http_429_retries_then_succeeds`; `http_500_exhausts_budget_then_fails_transport`; `http_400_is_invalid_request_without_retry`.
13. `no_retry_after_visible_output` — body breaks after a text delta: one request only, `Finished(Failed)`.
14. `setup_error_is_only_for_invalid_requests` — `stream()` returns `Err` ONLY when the request cannot be built or fails `validate` (kind `InvalidRequest`), before any network use. Every network-side failure, including connection refused on the first attempt, is reported as the stream's terminal `Finished(Failed)` (retries happen inside the stream).
15. `credentials_never_leak` — the fake bearer string appears in no `StreamEvent`, no `ProviderError`, no `Debug` of the provider.

Each adapter crate has one test `conformance()` calling `run_all`, plus its own route-specific
tests (golden request JSON, header sets, identity block, cache_control placement, thinking
config, stop-reason mapping, usage mapping, credential file handling).
