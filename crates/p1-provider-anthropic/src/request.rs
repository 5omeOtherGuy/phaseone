//! Pure request construction: body, headers, history mapping and thinking.
//!
//! Everything here is a pure function of the contracts and the composed model
//! policy, so it is golden-tested whole-body and byte-exact. Where this file and a
//! document disagree the document wins: the route's wire facts are
//! `docs/design/routes.md` §A, the policy split is `docs/design/routes-and-profiles.md`
//! §7.

use p1_contracts::history::{
    AssistantBlock, Item, Origin, ReplayData, ToolCall, ToolInput, ToolStatus,
};
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{ModelOptions, ProviderError, ProviderErrorKind, ProviderRequest};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use serde_json::{Value, json};

use crate::replay::{self, Replay, WireBlock};
use crate::{MessagesAccount, MessagesRoute};

/// `native` keys in this namespace are route-specific. None are known in this
/// slice, so any key here is rejected.
const NATIVE_PREFIX: &str = "anthropic-messages.";

/// Namespaces the OTHER compiled adapters own inside `ModelOptions::native`. An
/// explicit option from one of them was silently dropped on a route switch
/// before; it is now an error naming the option, this route and this adapter
/// (ADR-0039). Keys in no adapter's namespace keep their meaning: ignored.
const FOREIGN_NATIVE_PREFIXES: &[&str] = &["openai-responses.", "openai-chat."];

/// The request path, relative to the route's endpoint.
pub(crate) const MESSAGES_PATH: &str = "/v1/messages";

/// The account-mandated first system block. Without it the subscription account
/// rejects the request; it is wire behaviour, not part of any prompt file.
pub(crate) const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Betas every OAuth request carries.
pub(crate) const BASE_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Beta required only when the body carries a manual-budget `thinking` block.
pub(crate) const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Beta that lifts the Messages context window from 200k to 1M tokens. Sent only
/// on a route whose settings enable `long_context`: the window is an account and
/// model fact, so the route file decides it, not the request.
pub(crate) const LONG_CONTEXT_BETA: &str = "context-1m-2025-08-07";

/// `max_tokens` when the caller expresses no preference.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Headroom added to `max_tokens` when a manual thinking budget would otherwise
/// violate `budget_tokens < max_tokens`.
pub(crate) const MANUAL_OUTPUT_MARGIN: u32 = 8_192;

impl MessagesAccount {
    /// The identity block this account requires as the first system block.
    fn identity(self) -> &'static str {
        match self {
            MessagesAccount::ClaudeCodeSubscription => IDENTITY,
        }
    }

    /// The betas every request on this account carries.
    fn base_beta(self) -> &'static str {
        match self {
            MessagesAccount::ClaudeCodeSubscription => BASE_BETA,
        }
    }
}

/// `Effort` -> adaptive `output_config.effort`. `ExtraHigh` is the wire's
/// `xhigh`: the spelling is the protocol's, so it stays in the adapter.
fn adaptive_effort(effort: p1_contracts::Effort) -> &'static str {
    use p1_contracts::Effort;
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::ExtraHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// The file spelling of a thinking policy, for the refusal message.
fn policy_name(policy: ThinkingPolicy) -> &'static str {
    match policy {
        ThinkingPolicy::Enabled => "enabled",
        ThinkingPolicy::Preserved => "preserved",
        ThinkingPolicy::EffortLevel => "effort-level",
        ThinkingPolicy::Budget => "budget",
    }
}

fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

/// The model-dependent part of one Messages request: what the model profile's
/// policy makes of the request's options.
pub(crate) struct Lowered {
    /// `thinking`, absent when the profile resolves to no effort.
    pub thinking: Option<Value>,
    /// `output_config`, present only on the effort-level lane.
    pub output_config: Option<Value>,
    /// `max_tokens` when the request names no explicit cap.
    pub max_tokens: u32,
}

/// The ONE lowering function (ADR-0039, spec §7.3): profile policy × request
/// options -> the body's thinking fields and the output cap they require, or the
/// error that says why this combination cannot be expressed. The constructor,
/// `Provider::validate` and [`build_request`] all call it, so no rule about a
/// model lives anywhere else.
pub(crate) fn lower(
    profile: &ModelProfile,
    options: &ModelOptions,
) -> Result<Lowered, ProviderError> {
    let effort = profile.resolve_effort(options.reasoning_effort)?;
    match profile.thinking {
        ThinkingPolicy::EffortLevel => Ok(Lowered {
            thinking: effort.map(|_| json!({ "type": "adaptive", "display": "summarized" })),
            output_config: effort.map(|effort| json!({ "effort": adaptive_effort(effort) })),
            max_tokens: DEFAULT_MAX_TOKENS,
        }),
        ThinkingPolicy::Budget => {
            let Some(effort) = effort else {
                return Ok(Lowered {
                    thinking: None,
                    output_config: None,
                    max_tokens: DEFAULT_MAX_TOKENS,
                });
            };
            let budget = profile.budget_for(effort).ok_or_else(|| {
                invalid(&format!(
                    "profile `{}` carries no thinking budget for the requested effort",
                    profile.id
                ))
            })?;
            // The API requires `budget_tokens < max_tokens`. An EXPLICIT cap the
            // budget meets or exceeds cannot be honoured: reject it with the
            // smallest cap that would work, never raise it silently (ADR-0039).
            if let Some(cap) = options.max_output_tokens
                && budget >= cap
            {
                return Err(invalid(&format!(
                    "max_output_tokens {cap} leaves no room for the thinking budget {budget}: the \
                     Messages API requires budget_tokens < max_tokens, so the smallest cap that \
                     works is {} (or omit max_output_tokens and one is derived)",
                    budget + 1
                )));
            }
            Ok(Lowered {
                // The derived cap makes room for the budget rather than reducing it.
                max_tokens: if budget >= DEFAULT_MAX_TOKENS {
                    budget + MANUAL_OUTPUT_MARGIN
                } else {
                    DEFAULT_MAX_TOKENS
                },
                thinking: Some(json!({ "type": "enabled", "budget_tokens": budget })),
                output_config: None,
            })
        }
        ThinkingPolicy::Enabled | ThinkingPolicy::Preserved => Err(invalid(&format!(
            "the Messages adapter cannot express the profile's `thinking = \"{}\"` policy; it \
             encodes `effort-level` and `budget` only",
            policy_name(profile.thinking)
        ))),
    }
}

/// The one composition check, shared by the constructor and the pure request
/// builder: the route data is usable, the profile is valid, and the profile's
/// policy has an encoding here. Nothing is decided by a second, parallel table.
pub fn validate_composition(
    route: &MessagesRoute,
    wire_model: &str,
    profile: &ModelProfile,
) -> Result<(), ProviderError> {
    route.validate()?;
    profile.validate()?;
    if wire_model.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "wire model must be nonempty",
        ));
    }
    // The pure lowering decides: an `enabled`/`preserved` profile has no Messages
    // encoding, so it is refused here, at construction.
    lower(profile, &ModelOptions::default())?;
    Ok(())
}

/// Refuse what the composed route cannot carry, before a run starts: the
/// provider's `validate`, a function of the composition so a component shares it.
pub fn validate_request(
    route: &MessagesRoute,
    wire_model: &str,
    profile: &ModelProfile,
    request: &ProviderRequest,
) -> Result<(), ProviderError> {
    for tool in &request.tools {
        if matches!(tool.kind, DeclarationKind::Freeform { .. }) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!(
                    "tool `{}` is declared freeform, which route {} cannot carry",
                    tool.name, route.origin_route
                ),
            ));
        }
    }
    for key in request.options.native.keys() {
        if key.starts_with(NATIVE_PREFIX) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("unsupported route-native option `{key}`"),
            ));
        }
        if FOREIGN_NATIVE_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!(
                    "option \"{key}\" is not consumed by route \"{}\" \
                     (adapter anthropic-messages): it belongs to another adapter's namespace",
                    route.origin_route
                ),
            ));
        }
    }
    if request.options.cache_key.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("route {} takes no cache key", route.origin_route),
        ));
    }
    if request.options.max_output_tokens == Some(0) {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "max_output_tokens must be greater than zero",
        ));
    }
    // The model policy: the same lowering the request builder runs, so
    // `validate` can never accept a request the builder would reject.
    lower(profile, &request.options)?;
    // The history: everything else the Messages wire cannot carry is lowered
    // (a freeform call travels as `{"input": …}`), so the message mapping is
    // the check. A transcript whose first message would be an assistant turn
    // is refused by name before anything is sent (ADR-0049).
    build_messages(&route.origin_route, wire_model, &request.history)?;
    Ok(())
}

/// One request lowered for the wire WITHOUT any credential: what the native
/// provider sends, minus the `authorization` header [`build_headers`] adds.
#[derive(Clone, PartialEq, Eq)]
pub struct LoweredRequest {
    /// Appended to the route's endpoint.
    pub path: &'static str,
    /// Every header the native request sends except the credential, in its order.
    pub headers: Vec<(String, String)>,
    /// The encoded JSON body.
    pub body: Vec<u8>,
}

/// Validate and lower one request exactly as the native provider does before it
/// opens a transport, so both fail with the same error and send the same bytes.
pub fn lower_request(
    route: &MessagesRoute,
    wire_model: &str,
    profile: &ModelProfile,
    request: &ProviderRequest,
) -> Result<LoweredRequest, ProviderError> {
    validate_request(route, wire_model, profile, request)?;
    let body = build_request(route, wire_model, profile, request)?;
    let encoded = serde_json::to_vec(&body).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "the request body could not be encoded",
        )
    })?;
    let headers = build_headers_without_credential(route.account, &body);
    Ok(LoweredRequest {
        path: MESSAGES_PATH,
        headers: if route.long_context {
            with_long_context(headers)
        } else {
            headers
        },
        body: encoded,
    })
}

/// Translate a request into the Messages body. Pure: no credentials and no I/O.
///
/// Returns [`ProviderErrorKind::InvalidRequest`] for a route/profile pair the wire
/// cannot express, an effort the profile does not list, a cap that contradicts the
/// thinking budget, or a history that would start with an assistant turn (the API
/// requires the first message to be user-role).
pub fn build_request(
    route: &crate::MessagesRoute,
    wire_model: &str,
    profile: &ModelProfile,
    request: &ProviderRequest,
) -> Result<Value, ProviderError> {
    validate_composition(route, wire_model, profile)?;
    let lowered = lower(profile, &request.options)?;
    let max_tokens = request
        .options
        .max_output_tokens
        .unwrap_or(lowered.max_tokens);

    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), json!(wire_model));
    if let Some(thinking) = lowered.thinking {
        body.insert("thinking".to_string(), thinking);
    }
    if let Some(output_config) = lowered.output_config {
        body.insert("output_config".to_string(), output_config);
    }

    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), json!(true));
    body.insert(
        "system".to_string(),
        Value::Array(system_blocks(route.account, &request.system_prompt)),
    );
    body.insert(
        "messages".to_string(),
        Value::Array(build_messages(
            &route.origin_route,
            wire_model,
            &request.history,
        )?),
    );

    if !request.tools.is_empty() {
        let mut declarations = tool_declarations(&request.tools);
        if let Some(last) = declarations.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        body.insert("tools".to_string(), Value::Array(declarations));
    }

    // `cache_control` marks the prefix that should be cached: the last system
    // block and the last content block of the last user-role message.
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut)
        && let Some(message) = messages
            .iter_mut()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        && let Some(content) = message.get_mut("content").and_then(Value::as_array_mut)
        && let Some(block) = content.last_mut()
        && let Some(object) = block.as_object_mut()
    {
        object.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    }

    Ok(Value::Object(body))
}

/// `system` is a block array: the account's identity block first, then the prompt
/// block when the prompt is non-empty. The last block carries the cache marker so
/// the whole system prefix is cached.
fn system_blocks(account: MessagesAccount, prompt: &str) -> Vec<Value> {
    let mut blocks = vec![json!({ "type": "text", "text": account.identity() })];
    if !prompt.is_empty() {
        blocks.push(json!({ "type": "text", "text": prompt }));
    }
    if let Some(last) = blocks.last_mut() {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
    blocks
}

fn tool_declarations(tools: &[ToolDeclaration]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let schema = match &tool.kind {
                DeclarationKind::Function { input_schema } => input_schema.clone(),
                // `validate` rejects a freeform declaration before the request is
                // ever built; keep the shape total for the pure builder.
                DeclarationKind::Freeform { .. } => json!({}),
            };
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": schema,
            })
        })
        .collect()
}

/// Map the flat history onto strictly alternating messages. Adjacent same-role
/// items coalesce into one message's `content[]`; an assistant item whose blocks
/// all drop contributes no message at all. The ONLY history this route cannot
/// carry is one whose first message would be an assistant turn, so `validate`
/// calls this too and the two can never disagree.
pub(crate) fn build_messages(
    origin_route: &str,
    wire_model: &str,
    history: &[Item],
) -> Result<Vec<Value>, ProviderError> {
    let mut messages: Vec<Value> = Vec::new();
    // The configured origin every replay is classified against (ADR-0018): the
    // route's own route id and wire model, never a name a response echoed.
    let origin = Origin {
        route: origin_route.to_string(),
        model: wire_model.to_string(),
    };

    for item in history {
        match item {
            Item::User { text } => push_block(
                &mut messages,
                "user",
                json!({ "type": "text", "text": text }),
            ),
            // The inbox kind is a core-side concept; on the wire it is user input.
            Item::Inbox { text, .. } => push_block(
                &mut messages,
                "user",
                json!({ "type": "text", "text": text }),
            ),
            Item::ToolResult(result) => push_block(
                &mut messages,
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": result.content,
                    "is_error": result.status != ToolStatus::Ok,
                }),
            ),
            Item::Assistant(assistant) => {
                let mut blocks = Vec::new();
                for block in &assistant.blocks {
                    match block {
                        AssistantBlock::Text { text } if text.is_empty() => {}
                        AssistantBlock::Text { text } => {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        AssistantBlock::ToolCall(call) => blocks.push(tool_use_block(call)),
                        AssistantBlock::Reasoning { text, replay } => {
                            if let Some(replay) = replay_block(&origin, text, replay.as_ref())? {
                                blocks.push(replay);
                            }
                        }
                    }
                }
                if !blocks.is_empty() {
                    push_blocks(&mut messages, "assistant", blocks);
                }
            }
        }
    }

    if messages
        .first()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "the first message must be user-role: the history starts with an assistant turn",
        ));
    }

    Ok(messages)
}

/// A `tool_use` block carries a JSON object. A `Json` input is preserved when it
/// already is an object; anything else (invalid JSON, a non-object) becomes `{}` —
/// the call already failed at the tool, whose error result follows. A `Text` input
/// is a call of a kind this route has no shape for (a freeform call made on another
/// route), so it travels as a function-shaped call whose input is the object
/// `{"input": <raw text>}` (ADR-0049).
fn tool_use_block(call: &ToolCall) -> Value {
    let input = match &call.input {
        ToolInput::Json(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(object)) => Value::Object(object),
            _ => json!({}),
        },
        ToolInput::Text(raw) => json!({ "input": raw }),
    };
    json!({
        "type": "tool_use",
        "id": call.call_id,
        "name": call.name,
        "input": input,
    })
}

/// The ONE replay rule for one reasoning block, so `validate` and `build_request`
/// cannot disagree: a block of the configured origin that this build reads goes back
/// byte-exact, one another origin wrote is dropped entirely — never downgraded to
/// assistant text (ADR-0018) — and one of OUR origin that this build does not read is
/// refused, naming the item and both versions (ADR-0049).
fn replay_block(
    origin: &Origin,
    text: &str,
    replay: Option<&ReplayData>,
) -> Result<Option<Value>, ProviderError> {
    let Some(data) = replay else {
        return Ok(None);
    };
    match replay::decode(data, origin) {
        Replay::Foreign => Ok(None),
        Replay::Carried(WireBlock::Thinking { signature }) => Ok(Some(json!({
            "type": "thinking",
            "thinking": text,
            "signature": signature,
        }))),
        Replay::Carried(WireBlock::Redacted { data }) => {
            Ok(Some(json!({ "type": "redacted_thinking", "data": data })))
        }
        Replay::UnsupportedVersion { version } => Err(invalid(&format!(
            "cannot replay the reasoning block of the assistant item from {}/{}: its replay \
             data is version {version}, this route reads version {}",
            data.origin.route,
            data.origin.model,
            replay::REPLAY_VERSION
        ))),
        Replay::UnsupportedPayload => Err(invalid(&format!(
            "cannot replay the reasoning block of the assistant item from {}/{}: its replay \
             payload is not a thinking block",
            data.origin.route, data.origin.model
        ))),
    }
}

/// Append one block, coalescing into the previous message when the role matches.
fn push_block(messages: &mut Vec<Value>, role: &str, block: Value) {
    push_blocks(messages, role, vec![block]);
}

fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

/// Build the request headers this account requires from the credential and the
/// already-built body. The `anthropic-beta` set is payload-driven: the
/// interleaved-thinking beta is present exactly when the body carries a
/// manual-budget thinking block. This route never sends `x-api-key`.
#[cfg(feature = "native")]
pub fn build_headers(
    account: MessagesAccount,
    credential: &p1_provider_http::Credential,
    body: &Value,
) -> Vec<(String, String)> {
    let (mut headers, tail) = headers(account, body);
    headers.push((
        "authorization".to_string(),
        format!("Bearer {}", credential.bearer),
    ));
    headers.extend(tail);
    headers
}

/// The same header set for a route that sends NO credential (issue #134): an egress
/// proxy injects the credential, so the request carries no `authorization` header.
/// Everything else is byte for byte [`build_headers`]'s, because both are built
/// from the same halves.
pub fn build_headers_without_credential(
    account: MessagesAccount,
    body: &Value,
) -> Vec<(String, String)> {
    let (mut headers, tail) = headers(account, body);
    headers.extend(tail);
    headers
}

type Headers = Vec<(String, String)>;

/// The header set in two halves. [`build_headers`] puts the credential between
/// them, where it has always been, so a credential-free set never reorders the rest.
fn headers(account: MessagesAccount, body: &Value) -> (Headers, Headers) {
    let head = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "text/event-stream".to_string()),
        (
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        ),
        (
            "user-agent".to_string(),
            format!("p1/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    let mut tail = vec![
        (
            "anthropic-dangerous-direct-browser-access".to_string(),
            "true".to_string(),
        ),
        ("x-app".to_string(), "cli".to_string()),
    ];

    let mut beta = account.base_beta().to_string();
    if body
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("enabled")
    {
        beta.push(',');
        beta.push_str(INTERLEAVED_THINKING_BETA);
    }
    tail.push(("anthropic-beta".to_string(), beta));

    (head, tail)
}

/// Add the 1M-context beta to headers [`build_headers`] produced, for a route whose
/// settings enable `long_context`.
pub fn with_long_context(mut headers: Vec<(String, String)>) -> Vec<(String, String)> {
    if let Some((_, beta)) = headers
        .iter_mut()
        .find(|(name, _)| name == "anthropic-beta")
    {
        beta.push(',');
        beta.push_str(LONG_CONTEXT_BETA);
    }
    headers
}
