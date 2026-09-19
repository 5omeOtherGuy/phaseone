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
SSE decoder rules (donor tests port over): events split by a blank line; `\n`, `\r\n` and
`\r` line endings; several `data:` lines join with `\n`; one optional space after the colon is
stripped; `:` comment lines ignored; a chunk may end anywhere — in the middle of a line, of a
multi-byte UTF-8 character, or between `\r` and `\n`; `finish()` flushes a final event that
lacks the trailing blank line.

Status classification: `401|403` → `Reauth`; `408|425|429|500..=599` → `Retry`; any other
non-2xx → `Fatal`. `Retry-After` integer seconds honoured, clamped to 4 × the backoff cap.
`RetryPolicy { max_retries: 3, base: 2 s, cap: 60 s, jitter: ≤ 250 ms }`, doubling.

Retry loop invariants (each has a test, with a fake clock — `tokio::time::pause`, no real sleeps):
1. `Reauth` forces ONE credential refresh and one re-send per request; a second 401/403 is
   `ProviderErrorKind::Authentication`. It does not consume or reset the transient budget.
2. Transient failures (connect error, `Retry` status, stream broken BEFORE any content event
   was yielded) share one budget of `max_retries`.
3. **Never retry once any `TextDelta`/`ReasoningDelta`/`ToolInputDelta` has been yielded** —
   the failure becomes the terminal `Finished(Failed(Transport))`.
4. Back-off waits race cancellation and yield `StreamEvent::Activity` so the consumer sees life.
5. Error messages and logs carry status, error type and request id only — never a request or
   response body, never a header value.

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
- Usage mapping is in `routes.md`; absent fields stay `None`; `cost_micro_usd` is `None` on
  subscription routes.

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

## The ONE conformance suite — `p1-provider-conformance`

```rust
pub struct RouteUnderTest {
    pub name: &'static str,
    /// Build a provider wired to this scripted transport, with a fixed fake credential.
    pub build: fn(ScriptedTransport) -> Arc<dyn Provider>,
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
