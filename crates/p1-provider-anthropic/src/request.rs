//! Pure request construction: body, headers, history mapping and thinking.
//!
//! Everything here is a pure function of the contracts, so it is golden-tested
//! whole-body and byte-exact. The route's wire facts are `docs/design/routes.md`
//! §A; where this file and that document disagree the document wins.

use p1_contracts::history::{AssistantBlock, Item, ReplayData, ToolCall, ToolStatus};
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{ProviderError, ProviderErrorKind, ProviderRequest};
use serde_json::{Value, json};

use crate::ROUTE;

/// The route-mandated first system block. Without it the subscription route
/// rejects the request; it is wire behaviour, not part of any prompt file.
pub(crate) const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Betas every OAuth request carries.
pub(crate) const BASE_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Beta required only when the body carries a manual-budget `thinking` block.
pub(crate) const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// `max_tokens` when the caller expresses no preference.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Headroom added to `max_tokens` when a manual thinking budget would otherwise
/// violate `budget_tokens < max_tokens`.
pub(crate) const MANUAL_OUTPUT_MARGIN: u32 = 8_192;

/// The smallest thinking budget the API accepts.
pub(crate) const MIN_THINKING_BUDGET: u32 = 1_024;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Model ids with a server-side adaptive effort control instead of a manual
/// `budget_tokens`. Prefix match so dated snapshots (`claude-opus-5-2026…`)
/// resolve to the same lane.
fn is_adaptive(model: &str) -> bool {
    const ADAPTIVE_PREFIXES: [&str; 3] = ["claude-fable-5", "claude-opus-5", "claude-sonnet-5"];
    ADAPTIVE_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

/// `Effort` -> adaptive `output_config.effort`. `ExtraHigh` is the wire's
/// `xhigh`.
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

/// `Effort` -> manual `thinking.budget_tokens`. The map is the donor's
/// minimalcc-pi budget table; `ExtraHigh` and `Max` share the 32768 ceiling.
fn manual_budget(effort: p1_contracts::Effort) -> u32 {
    use p1_contracts::Effort;
    match effort {
        Effort::Low => 4_096,
        Effort::Medium => 10_240,
        Effort::High => 20_480,
        Effort::ExtraHigh | Effort::Max => 32_768,
    }
}

/// The conflict an EXPLICIT output cap can have with a manual thinking budget:
/// the API requires `budget_tokens < max_tokens`, so a cap the budget meets or
/// exceeds cannot be honoured. Rejected by `validate` and `build_request` with
/// the smallest cap that would work (ADR-0039); an unspecified cap is never in
/// conflict — it is derived in [`build_request`] as before.
pub(crate) fn explicit_cap_conflict(
    model: &str,
    options: &p1_contracts::ModelOptions,
) -> Option<ProviderError> {
    let effort = options.reasoning_effort?;
    let cap = options.max_output_tokens?;
    if is_adaptive(model) {
        return None;
    }
    let budget = manual_budget(effort);
    (budget >= cap).then(|| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!(
                "max_output_tokens {cap} leaves no room for the thinking budget {budget}: the \
                 Messages API requires budget_tokens < max_tokens, so the smallest cap that works \
                 is {} (or omit max_output_tokens and one is derived)",
                budget + 1
            ),
        )
    })
}

/// Translate a request into the Messages body. Pure: no credentials, no I/O and
/// no route validation (that is [`AnthropicProvider::validate`]).
///
/// Returns [`ProviderErrorKind::InvalidRequest`] when the history would start an
/// assistant turn: the API requires the first message to be user-role.
pub fn build_request(model: &str, request: &ProviderRequest) -> Result<Value, ProviderError> {
    if let Some(error) = explicit_cap_conflict(model, &request.options) {
        return Err(error);
    }
    let mut max_tokens = request
        .options
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), json!(model));

    if let Some(effort) = request.options.reasoning_effort {
        if is_adaptive(model) {
            body.insert(
                "thinking".to_string(),
                json!({ "type": "adaptive", "display": "summarized" }),
            );
            body.insert(
                "output_config".to_string(),
                json!({ "effort": adaptive_effort(effort) }),
            );
        } else {
            let budget = manual_budget(effort);
            debug_assert!(
                budget >= MIN_THINKING_BUDGET,
                "manual budget below the API floor"
            );
            // The API rejects `budget_tokens >= max_tokens`; raise the output cap
            // rather than reducing the requested thinking budget. An EXPLICIT cap
            // this would swallow was rejected above; this is the derived default.
            if budget >= max_tokens {
                max_tokens = budget + MANUAL_OUTPUT_MARGIN;
            }
            body.insert(
                "thinking".to_string(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
        }
    }

    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), json!(true));
    body.insert(
        "system".to_string(),
        Value::Array(system_blocks(&request.system_prompt)),
    );
    body.insert(
        "messages".to_string(),
        Value::Array(build_messages(model, &request.history)?),
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

/// `system` is a block array: the mandatory identity block first, then the
/// prompt block when the prompt is non-empty. The last block carries the cache
/// marker so the whole system prefix is cached.
fn system_blocks(prompt: &str) -> Vec<Value> {
    let mut blocks = vec![json!({ "type": "text", "text": IDENTITY })];
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
/// all drop contributes no message at all.
fn build_messages(model: &str, history: &[Item]) -> Result<Vec<Value>, ProviderError> {
    let mut messages: Vec<Value> = Vec::new();

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
                            if let Some(replay) = replay_block(model, text, replay.as_ref()) {
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

/// A `tool_use` block carries a JSON object. The raw input is preserved when it
/// already is one; anything else (invalid JSON, a non-object, a freeform string)
/// becomes `{}`. The call already failed at the tool, whose error result follows.
fn tool_use_block(call: &ToolCall) -> Value {
    let input = match serde_json::from_str::<Value>(call.input.raw()) {
        Ok(Value::Object(object)) => Value::Object(object),
        _ => json!({}),
    };
    json!({
        "type": "tool_use",
        "id": call.call_id,
        "name": call.name,
        "input": input,
    })
}

/// A reasoning block replays only when the origin is this exact route + model
/// and the payload version is current. A foreign or stale-version block is
/// dropped entirely — never downgraded to assistant text.
fn replay_block(model: &str, text: &str, replay: Option<&ReplayData>) -> Option<Value> {
    let replay = replay?;
    if replay.version != 1 {
        return None;
    }
    if replay.origin.route != ROUTE || replay.origin.model != model {
        return None;
    }
    match replay.payload.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            let signature = replay.payload.get("signature").and_then(Value::as_str)?;
            Some(json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            }))
        }
        Some("redacted_thinking") => {
            let data = replay.payload.get("data").and_then(Value::as_str)?;
            Some(json!({ "type": "redacted_thinking", "data": data }))
        }
        _ => None,
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

/// Build the OAuth request headers from the credential and the already-built
/// body. The `anthropic-beta` set is payload-driven: the interleaved-thinking
/// beta is present exactly when the body carries a manual-budget thinking block.
/// This route never sends `x-api-key`.
pub fn build_headers(
    credential: &p1_provider_http::Credential,
    body: &Value,
) -> Vec<(String, String)> {
    let mut headers = vec![
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
        (
            "authorization".to_string(),
            format!("Bearer {}", credential.bearer),
        ),
        (
            "anthropic-dangerous-direct-browser-access".to_string(),
            "true".to_string(),
        ),
        ("x-app".to_string(), "cli".to_string()),
    ];

    let mut beta = BASE_BETA.to_string();
    if body
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("enabled")
    {
        beta.push(',');
        beta.push_str(INTERLEAVED_THINKING_BETA);
    }
    headers.push(("anthropic-beta".to_string(), beta));

    headers
}
