//! The WebSocket decisions of the Responses provider: the portable half of the
//! route-bound WebSocket (ADR-0078 §1, `docs/design/modules/wit.md` decision
//! S0-R2.2).
//!
//! [`WebSocketDecisions::lower`] is WIT `provider.lower(request, connection-state)`
//! for a route that speaks WebSocket: from the request and the facts the host session
//! reports in [`ConnectionState`] it returns [`Lowered::Http`] with the frozen
//! `http.http-request` record ([`LoweredHttpRequest`]), the fallback, or
//! [`Lowered::WebSocket`] with a [`WebSocketSend`] — a handshake head exactly when no
//! connection is open, and the ONE text frame: the full request frame, or the shorter
//! continuation frame (`previous_response_id` plus only the new items) that
//! `docs/design/websocket.md` §6 allows.
//!
//! What lives here is component state: the fact that this instance fell back to
//! HTTP and speaks it from then on, and §6's memory of the last response it
//! completed. That memory is only ever used on the connection the host reports open
//! with that very response as its last clean one, so every drop, every reconnect
//! and every fallback — which the host reports as `open = false` or a different
//! response — clears it.
//!
//! PORTABLE: no `cfg`, no clock, no socket, no async runtime and no credential; it
//! depends on `p1-contracts` alone (its `serde_json`), so a provider WebAssembly
//! component can run it. The full-frame encoder is handed in
//! ([`WebSocketDecisions::new`]): it is the request builder's, which lives beside
//! this file.

use p1_contracts::serde_json::{self, Map, Value, json};

/// WIT `websocket.connection-state`: the host's facts, and nothing it decided.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionState {
    /// A WebSocket connection of this provider instance is open.
    pub open: bool,
    /// The response id of the last response that open connection completed cleanly.
    pub last_clean_response: Option<String>,
    /// The previous attempt of this same request failed over WebSocket before any
    /// output, with no further WebSocket attempt allowed by `websocket.md` §5.
    pub failed_before_output: bool,
}

/// WIT `websocket.websocket-request-head`: what opens a connection. It names where
/// the host attaches the credential (always a bearer; `account_id_header` when the
/// account needs one), never a credential.
#[derive(Clone, PartialEq, Eq)]
pub struct WebSocketHead {
    /// Relative to the route's endpoint; empty is the endpoint itself.
    pub path: String,
    /// Handshake headers without any credential.
    pub headers: Vec<(String, String)>,
    /// The header the host fills with the credential's account id.
    pub account_id_header: Option<String>,
}

impl std::fmt::Debug for WebSocketHead {
    /// Header names only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("WebSocketHead")
            .field("path_len", &self.path.len())
            .field("header_names", &names)
            .field("account_id_header", &self.account_id_header)
            .finish()
    }
}

/// WIT `websocket.websocket-send`.
#[derive(Clone, PartialEq, Eq)]
pub struct WebSocketSend {
    /// Present exactly when [`ConnectionState::open`] was false.
    pub handshake: Option<WebSocketHead>,
    /// The one text frame.
    pub frame: String,
}

impl std::fmt::Debug for WebSocketSend {
    /// The frame holds the conversation: only its length is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketSend")
            .field("handshake", &self.handshake)
            .field("frame_len", &self.frame.len())
            .finish()
    }
}

/// WIT `http.http-request` (`modules/wit/transport.wit`): the request the fallback
/// arm sends, as the frozen record names it — the same record
/// `p1_provider_http::LoweredHttpRequest` mirrors on the native broker side. The
/// method is always POST, the only method the frozen interface has, and no
/// credential is here: [`Self::account_id_header`] names where the host attaches the
/// route's own.
#[derive(Clone, PartialEq, Eq)]
pub struct LoweredHttpRequest {
    /// Path and query relative to the route's endpoint, starting with `/`.
    pub path: String,
    /// Headers without any credential, in the order they go out.
    pub headers: Vec<(String, String)>,
    /// The header the host fills with the credential's account id, for an account that
    /// needs one.
    pub account_id_header: Option<String>,
    /// The encoded JSON body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for LoweredHttpRequest {
    /// Header names and lengths only: the body holds the conversation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("LoweredHttpRequest")
            .field("path_len", &self.path.len())
            .field("header_names", &names)
            .field("account_id_header", &self.account_id_header)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// WIT `provider.lowered-request` for a WebSocket route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lowered {
    /// Send the request over HTTP/SSE: the record the request builder makes for an
    /// SSE route, unchanged.
    Http(LoweredHttpRequest),
    WebSocket(WebSocketSend),
}

/// The decision state of one provider instance.
pub struct WebSocketDecisions {
    /// The full request frame for a body (`docs/design/websocket.md` §3).
    full_frame: fn(&Value) -> String,
    /// Set by the first fallback: this instance speaks HTTP from then on (§5).
    turned_off: bool,
    /// §6: what the last cleanly completed response lets the next request continue.
    memory: Option<Memory>,
}

impl WebSocketDecisions {
    /// A fresh instance: WebSocket on, nothing to continue from.
    pub fn new(full_frame: fn(&Value) -> String) -> Self {
        Self {
            full_frame,
            turned_off: false,
            memory: None,
        }
    }

    /// Whether a fallback has turned WebSocket off for this instance.
    pub fn is_turned_off(&self) -> bool {
        self.turned_off
    }

    /// Lower one attempt of the request whose body is `body`.
    ///
    /// `head` and `http` are the rest of the request, as the host derived it from the
    /// route: the handshake a new connection opens with, and the frozen
    /// `http.http-request` record the fallback arm returns unchanged.
    ///
    /// - `Http(http)` once this instance has fallen back, and whenever the host reports
    ///   `failed_before_output` — which also turns WebSocket off for good;
    /// - otherwise a send with `head` exactly when no connection is open, and the
    ///   continuation frame only when the open connection's last clean response is
    ///   the one this instance remembers and the body continues it (§6); every
    ///   other case is the full frame.
    pub fn lower(
        &mut self,
        body: &Value,
        head: WebSocketHead,
        http: &LoweredHttpRequest,
        state: &ConnectionState,
    ) -> Lowered {
        if self.turned_off || state.failed_before_output {
            self.turned_off = true;
            self.memory = None;
            return Lowered::Http(http.clone());
        }
        if !state.open {
            // A new connection has answered nothing yet: §6's memory is per
            // connection, so its first frame is necessarily a FULL body.
            self.memory = None;
            return Lowered::WebSocket(WebSocketSend {
                handshake: Some(head),
                frame: (self.full_frame)(body),
            });
        }
        let continuation = self
            .memory
            .as_ref()
            .filter(|memory| state.last_clean_response.as_deref() == Some(&memory.response_id))
            .and_then(|memory| continuation_body(memory, body));
        Lowered::WebSocket(WebSocketSend {
            handshake: None,
            frame: (self.full_frame)(continuation.as_ref().unwrap_or(body)),
        })
    }

    /// The response to the request whose body is `body` completed cleanly and
    /// reported `facts`: remember what the next request may continue from. Nothing
    /// is remembered when the stream named no response id or the body has no
    /// `input` array; the next request then sends the FULL body, never an error.
    pub fn completed(&mut self, body: &Value, facts: &ResponseFacts) {
        self.memory = remember(body, facts);
    }
}

/// §6: what one connection remembers about the response it completed last. Each
/// field is read by exactly one of §6's rules.
pub(crate) struct Memory {
    /// The FULL body that response answered. Its `input` array is the prefix rule 3
    /// requires the next `input` to start with, and every other top-level field is
    /// what rule 2 compares. A continuation's own body IS this body — rule 3 makes
    /// its `input` the same array — so the memory stays valid turn after turn.
    pub(crate) body: Value,
    /// The id the continuation sends as `previous_response_id` (§6 rule 1).
    pub(crate) response_id: String,
    /// The output items of that response, in order, as the next request's `input`
    /// re-encodes them. They are where the echo rule 3 requires ends.
    pub(crate) items: Vec<EchoedItem>,
}

impl std::fmt::Debug for Memory {
    /// Lengths and the response id only: the remembered body holds the whole
    /// conversation, and no `Debug` output may carry it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let input_items = self
            .body
            .get("input")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        f.debug_struct("Memory")
            .field("input_items", &input_items)
            .field("output_items", &self.items.len())
            .field("response_id", &self.response_id)
            .finish()
    }
}

/// One output item of a remembered response, reduced to the fields the next
/// request's `input` re-encodes of it (§6 rule 3).
#[derive(Clone)]
pub(crate) struct EchoedItem {
    /// The `type` the next request writes for this item.
    pub(crate) kind: &'static str,
    /// The `role` it writes, for a message: what tells the echoed assistant message
    /// from the user message that follows it.
    pub(crate) role: Option<&'static str>,
    /// The item's wire `id`. The encoding does not carry one, so it is only
    /// compared when a new item happens to have one.
    pub(crate) id: Option<String>,
    /// The call id the next request writes — `call_id`, or the `id` the wire put it
    /// there instead (the parser's own rule, so the two cannot disagree).
    pub(crate) call_id: Option<String>,
}

impl EchoedItem {
    /// Whether `item` is THIS remembered output item as the next request encodes it.
    /// `type` and `role` are what the encoding writes, and a call's id comes back
    /// under either of the wire's two spellings (the parser's rule). A plain item
    /// `id` is NOT re-encoded: one is compared only when a new item has one.
    pub(crate) fn answers(&self, item: &Value) -> bool {
        if item.get("type").and_then(Value::as_str) != Some(self.kind) {
            return false;
        }
        item.get("role").and_then(Value::as_str) == self.role
            && self
                .call_id
                .as_deref()
                .is_none_or(|call_id| item_call_id(item) == call_id)
            && item
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| self.id.as_deref() == Some(id))
    }
}

/// The output item a completed response would contribute to the next request's
/// `input`, or `None` for one it would not contribute at all: the parser only
/// builds a block for the four known item types, `input_items` skips an empty
/// assistant text and a reasoning item without encrypted content, and an unknown
/// type is nothing on both sides. Only items that DO come back are part of the
/// echo rule 3 looks for.
pub(crate) fn echoed_item(item: &Value) -> Option<EchoedItem> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    match kind {
        "message" => has_output_text(item).then_some(EchoedItem {
            kind: "message",
            role: Some("assistant"),
            id,
            call_id: None,
        }),
        "reasoning" => item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|encrypted| !encrypted.is_empty())
            .then_some(EchoedItem {
                kind: "reasoning",
                role: None,
                id,
                call_id: None,
            }),
        "function_call" => Some(EchoedItem {
            kind: "function_call",
            role: None,
            id,
            call_id: Some(item_call_id(item)),
        }),
        "custom_tool_call" => Some(EchoedItem {
            kind: "custom_tool_call",
            role: None,
            id,
            call_id: Some(item_call_id(item)),
        }),
        _ => None,
    }
}

/// Whether a message item carries any `output_text` (the only part kind that
/// becomes a text block, and therefore the next request's assistant message).
fn has_output_text(item: &Value) -> bool {
    item.get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content.iter().any(|part| {
                part.get("type").and_then(Value::as_str) == Some("output_text")
                    && part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
            })
        })
}

/// The call id the parser would take from this item, so the fingerprint and the
/// re-encoded `call_id` are the same string.
fn item_call_id(item: &Value) -> String {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The response id and the re-encodable output items ONE attempt's stream has
/// reported, in arrival order. Recorded per frame, read when the response ends.
#[derive(Default)]
pub struct ResponseFacts {
    id: Option<String>,
    items: Vec<EchoedItem>,
}

impl ResponseFacts {
    /// Read the two envelope facts §6 needs out of one event frame. The parser stays
    /// the only reader of the event VOCABULARY (`docs/design/websocket.md` §3: no
    /// second parser); this reads only the fields the continuation rules name, and a
    /// frame it cannot read changes nothing.
    pub fn record(&mut self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response.created")
            | Some("response.completed")
            | Some("response.done")
            | Some("response.incomplete") => {
                if let Some(id) = value
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                {
                    self.id = Some(id.to_string());
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item")
                    && let Some(echoed) = echoed_item(item)
                {
                    self.items.push(echoed);
                }
            }
            _ => {}
        }
    }

    /// The response id the stream reported (WIT `decoder.response-id`).
    pub fn response_id(&self) -> Option<&str> {
        self.id.as_deref()
    }
}

/// §6: what to remember now that the response ended cleanly.
fn remember(body: &Value, facts: &ResponseFacts) -> Option<Memory> {
    let response_id = facts.id.clone()?;
    body.get("input")?.as_array()?;
    Some(Memory {
        body: body.clone(),
        response_id,
        items: facts.items.clone(),
    })
}

/// §6 rules 2 and 3, and what a continuation sends: the body to send with
/// `previous_response_id` and only the new items, or `None` for the FULL body.
///
/// Rule 3 is decided on JSON VALUES, in three steps:
/// 1. the new `input` starts with the remembered `input` (call it A);
/// 2. the items after A ARE the output items the remembered response completed, in
///    order, as this adapter's `input_items` re-encodes them — the echo;
/// 3. at least one further item follows the echo, and those are the items to send.
///
/// Any step failing means the FULL body. Rule 1 holds before this is ever reached:
/// the memory is only used on the open connection whose last clean response it is.
fn continuation_body(memory: &Memory, body: &Value) -> Option<Value> {
    if !same_shape(&memory.body, body) {
        return None;
    }
    let base = memory.body.get("input")?.as_array()?;
    let input = body.get("input")?.as_array()?;
    let echo = input.get(base.len()..)?;
    if input.get(..base.len())? != base.as_slice() || echo.len() <= memory.items.len() {
        return None;
    }
    for (item, expected) in echo.iter().zip(&memory.items) {
        if !expected.answers(item) {
            return None;
        }
    }
    let mut continuation = body.clone();
    let fields = continuation.as_object_mut()?;
    fields.insert(
        "input".to_string(),
        Value::Array(echo[memory.items.len()..].to_vec()),
    );
    fields.insert(
        "previous_response_id".to_string(),
        json!(memory.response_id),
    );
    Some(continuation)
}

/// §6 rule 2: every top-level field of the new body except `input` equals the
/// remembered one. The key SET counts: a field that appeared or vanished is a
/// different shape, and then the full body goes out.
fn same_shape(remembered: &Value, body: &Value) -> bool {
    fn fields(value: &Value) -> Option<Map<String, Value>> {
        let mut fields = value.as_object()?.clone();
        fields.remove("input");
        Some(fields)
    }
    match (fields(remembered), fields(body)) {
        (Some(remembered), Some(body)) => remembered == body,
        _ => false,
    }
}
