use crate::SubscriptionRoute;
use p1_contracts::{
    AssistantBlock, DeclarationKind, Effort, Item, ProviderError, ProviderErrorKind,
    ProviderRequest, ToolInput,
};
use serde_json::{Value, json};

pub(crate) fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

pub(crate) fn validate(
    route: SubscriptionRoute,
    request: &ProviderRequest,
) -> Result<(), ProviderError> {
    if request
        .tools
        .iter()
        .any(|t| matches!(t.kind, DeclarationKind::Freeform { .. }))
    {
        return Err(invalid(
            "chat subscription routes support function tools only",
        ));
    }
    if matches!(
        request.options.reasoning_effort,
        Some(Effort::Medium | Effort::ExtraHigh | Effort::Low)
    ) {
        return Err(invalid(
            "this subscription route supports high or max reasoning effort",
        ));
    }
    if request.options.max_output_tokens == Some(0) {
        return Err(invalid("output cap must be positive"));
    }
    if let Some(key) = &request.options.cache_key {
        if route == SubscriptionRoute::Glm {
            return Err(invalid(
                "GLM has automatic prefix caching, no explicit cache key",
            ));
        }
        if key.is_empty() || key.len() > 256 || !key.bytes().all(|b| (33..=126).contains(&b)) {
            return Err(invalid(
                "cache key must be 1..256 printable ASCII bytes without spaces",
            ));
        }
    }
    if request
        .options
        .native
        .keys()
        .any(|key| key.starts_with("openai-chat.") || key.starts_with(&format!("{}.", route.id())))
    {
        return Err(invalid("unknown native option in chat route namespace"));
    }
    Ok(())
}

/// Pure wire builder. Foreign reasoning is dropped, never rendered as assistant text.
pub fn build_request(
    route: SubscriptionRoute,
    model: &str,
    request: &ProviderRequest,
) -> Result<Value, ProviderError> {
    validate(route, request)?;
    let origin = route.origin(model);
    let mut messages = vec![json!({"role":"system", "content": request.system_prompt})];
    for item in &request.history {
        match item {
            Item::User { text } | Item::Inbox { text, .. } => {
                messages.push(json!({"role":"user", "content":text}))
            }
            Item::ToolResult(result) => messages.push(
                json!({"role":"tool", "tool_call_id":result.call_id, "content":result.content}),
            ),
            Item::Assistant(item) => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut has_reasoning = false;
                let mut calls = Vec::new();
                for block in &item.blocks {
                    match block {
                        AssistantBlock::Text { text: part } => text.push_str(part),
                        AssistantBlock::Reasoning {
                            replay: Some(data), ..
                        } if data.origin == origin => {
                            if data.version != 1 {
                                return Err(invalid("unsupported reasoning replay version"));
                            }
                            let part = data
                                .payload
                                .as_str()
                                .ok_or_else(|| invalid("invalid reasoning replay payload"))?;
                            reasoning.push_str(part);
                            has_reasoning = true;
                        }
                        AssistantBlock::Reasoning { .. } => {}
                        AssistantBlock::ToolCall(call) => {
                            let ToolInput::Json(raw) = &call.input else {
                                return Err(invalid(
                                    "cannot replay freeform tool input on a function route",
                                ));
                            };
                            calls.push(json!({"id":call.call_id,"type":"function","function":{"name":call.name,"arguments":raw}}));
                        }
                    }
                }
                let mut message = json!({"role":"assistant","content":text});
                if has_reasoning {
                    message["reasoning_content"] = json!(reasoning);
                }
                if !calls.is_empty() {
                    message["tool_calls"] = json!(calls);
                }
                messages.push(message);
            }
        }
    }
    let mut body = json!({"model":model,"messages":messages,"stream":true,"stream_options":{"include_usage":true},"reasoning_effort":match request.options.reasoning_effort {Some(Effort::Max)=>"max", _=>"high"}});
    body["thinking"] = match route {
        SubscriptionRoute::OpenCodeGo => json!({"type":"enabled"}),
        SubscriptionRoute::Glm => json!({"type":"enabled","clear_thinking":false}),
    };
    if let Some(cap) = request.options.max_output_tokens {
        body["max_tokens"] = json!(cap);
    }
    if !request.tools.is_empty() {
        body["tools"] = request.tools.iter().map(|tool| match &tool.kind {
            DeclarationKind::Function { input_schema } => json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":input_schema}}),
            _ => unreachable!("validated"),
        }).collect();
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{
        AssistantItem, ModelOptions, ReplayData, ToolCall, ToolDeclaration, ToolResultItem,
        ToolStatus,
    };
    fn request() -> ProviderRequest {
        ProviderRequest {
            system_prompt: "system".into(),
            history: vec![],
            tools: vec![],
            options: ModelOptions::default(),
        }
    }
    #[test]
    fn golden_tool_round_trip_and_subscription_options() {
        for route in [SubscriptionRoute::OpenCodeGo, SubscriptionRoute::Glm] {
            let mut r = request();
            let origin = route.origin("model");
            r.tools.push(ToolDeclaration {
                name: "read".into(),
                description: "Read".into(),
                kind: DeclarationKind::Function {
                    input_schema: json!({"type":"object"}),
                },
            });
            r.history = vec![
                Item::User {
                    text: "inspect".into(),
                },
                Item::Assistant(AssistantItem {
                    origin: origin.clone(),
                    blocks: vec![
                        AssistantBlock::Reasoning {
                            text: "display must not replay".into(),
                            replay: Some(ReplayData {
                                origin,
                                version: 1,
                                payload: json!("exact\n雪"),
                            }),
                        },
                        AssistantBlock::ToolCall(ToolCall {
                            call_id: "call".into(),
                            name: "read".into(),
                            input: ToolInput::Json("{ \"path\": \"a\" }".into()),
                        }),
                    ],
                }),
                Item::ToolResult(ToolResultItem {
                    call_id: "call".into(),
                    name: "read".into(),
                    status: ToolStatus::Error,
                    content: "not found".into(),
                }),
            ];
            r.options.max_output_tokens = Some(200);
            let body = build_request(route, "model", &r).unwrap();
            assert_eq!(
                body,
                json!({"model":"model","stream":true,"stream_options":{"include_usage":true},"reasoning_effort":"high","max_tokens":200,
                    "thinking":if route==SubscriptionRoute::Glm {json!({"type":"enabled","clear_thinking":false})}else{json!({"type":"enabled"})},
                    "tools":[{"type":"function","function":{"name":"read","description":"Read","parameters":{"type":"object"}}}],
                    "messages":[{"role":"system","content":"system"},{"role":"user","content":"inspect"},{"role":"assistant","content":"","reasoning_content":"exact\n雪","tool_calls":[{"id":"call","type":"function","function":{"name":"read","arguments":"{ \"path\": \"a\" }"}}]},{"role":"tool","tool_call_id":"call","content":"not found"}]
                })
            );
            let foreign = build_request(route, "different-model", &r)
                .unwrap()
                .to_string();
            assert!(!foreign.contains("exact"));
            assert!(!foreign.contains("display must"));
        }
    }
    #[test]
    fn rejects_unrepresentable_options_and_tools() {
        let route = SubscriptionRoute::OpenCodeGo;
        let mut r = request();
        r.tools.push(ToolDeclaration {
            name: "patch".into(),
            description: String::new(),
            kind: DeclarationKind::Freeform { grammar: None },
        });
        assert!(build_request(route, "m", &r).is_err());
        r.tools.clear();
        for effort in [Effort::Low, Effort::Medium, Effort::ExtraHigh] {
            r.options.reasoning_effort = Some(effort);
            assert!(build_request(route, "m", &r).is_err());
        }
        r.options = ModelOptions::default();
        r.options
            .native
            .insert("openai-chat.typo".into(), json!(true));
        assert!(build_request(route, "m", &r).is_err());
        r.options.native.clear();
        r.options.native.insert("other.option".into(), json!(true));
        assert!(build_request(route, "m", &r).is_ok());
        r.options.cache_key = Some("bad\nheader".into());
        assert!(build_request(route, "m", &r).is_err());
        r.options.cache_key = Some("valid".into());
        assert!(build_request(route, "m", &r).is_ok());
        assert!(build_request(SubscriptionRoute::Glm, "m", &r).is_err());
    }
}
