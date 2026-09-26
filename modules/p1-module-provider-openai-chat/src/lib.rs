//! `p1/provider-openai-chat`: the OpenAI Chat Completions routes (OpenCode Go and Zen, Z.ai
//! and the others of the `openai-chat` adapter) as a provider component (ADR-0071).
//!
//! It implements the `provider` world of `modules/wit/` over the portable half of
//! `p1-provider-openai-chat` (S4.3.1), so the component validates, lowers, classifies and
//! decodes with the same code the native `ChatProvider` runs. It never sends: the
//! native transport broker sends the lowered request, attaches the credential, retries and
//! frames the SSE body, and feeds the events to the `decoder` (freeze item 9).
//!
//! One instance holds one composition, stored by `configure`. Every export before
//! `configure` answers a `Protocol` error rather than trapping. A decoder fed after its
//! terminal event traps, which the host maps to `Protocol` (`decoding.wit`).
#![forbid(unsafe_code)]

mod settings;

use std::cell::{OnceCell, RefCell};

use p1_bindings_provider::generated::exports::p1::module::decoding::{
    Guest as DecodingGuest, GuestDecoder, WireEvent,
};
use p1_bindings_provider::generated::p1::module::credential_control::{
    CredentialScheme, CredentialUse,
};
use p1_bindings_provider::generated::p1::module::http::{HttpRequest, Method, ResponseHead};
use p1_bindings_provider::generated::p1::module::types::{Declaration, DeclarationKind};
use p1_bindings_provider::generated::{
    ConnectionState, Guest, LoweredRequest, ProviderRequest, ProviderSettings,
};
use p1_contracts::tool::{Grammar, ToolDeclaration};
use p1_contracts::{
    Item, ModelOptions, Outcome, ProviderError, ProviderErrorKind,
    ProviderRequest as ContractRequest, StreamEvent,
};
use p1_module_protocol::{
    WireItem, WireModelOptions, WireProviderError, WireRouteDescription, WireStreamEvent,
};
use p1_provider_http::{ResponseParser, SseEvent};
use p1_provider_openai_chat::{ChatParser, lower_request, validate_request};

use settings::Composition;

thread_local! {
    /// The instance's one composition. The component model runs an instance on one thread
    /// and calls it once at a time, so a thread-local cell is the whole instance's state.
    static COMPOSITION: OnceCell<Composition> = const { OnceCell::new() };
}

const NOT_CONFIGURED: &str = "the provider module was used before configure";
const CONFIGURED_TWICE: &str = "the provider module was configured twice";
const BAD_HISTORY: &str = "the request carries a history item that is not protocol JSON";
const BAD_OPTIONS: &str = "the request carries model options that are not protocol JSON";
const BAD_SCHEMA: &str = "the request carries a tool input schema that is not JSON";
/// The panic message of a decoder used after its terminal event: the trap it causes is
/// what the host reports, and the message never leaves the guest.
const USED_AFTER_TERMINAL: &str = "decoder used after its terminal event";

fn protocol(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, message)
}

/// Runs `f` on the stored composition, or fails with `Protocol` before `configure`.
fn with_composition<T>(
    f: impl FnOnce(&Composition) -> Result<T, ProviderError>,
) -> Result<T, ProviderError> {
    COMPOSITION.with(|cell| match cell.get() {
        Some(composition) => f(composition),
        None => Err(protocol(NOT_CONFIGURED)),
    })
}

/// A protocol value as its JSON text. Every `Wire*` value is plain data (strings, numbers,
/// JSON values), so serializing it cannot fail; if it ever did, the trap is the right
/// answer, since the host maps it to `Protocol`.
fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("a protocol value serializes")
}

fn error_json(error: ProviderError) -> String {
    json(&WireProviderError::from(error))
}

fn event_json(event: StreamEvent) -> String {
    json(&WireStreamEvent::from(event))
}

/// The contract request a `provider-request` carries. A value that is not the protocol's
/// JSON is the host's broken message, so it is a `Protocol` error with a constant text that
/// quotes nothing of it.
fn decode_request(request: ProviderRequest) -> Result<ContractRequest, ProviderError> {
    let history = request
        .history
        .iter()
        .map(|item| serde_json::from_str::<WireItem>(item).map(Item::from))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| protocol(BAD_HISTORY))?;
    let tools = request
        .tools
        .into_iter()
        .map(declaration)
        .collect::<Result<Vec<_>, _>>()?;
    let options = serde_json::from_str::<WireModelOptions>(&request.options)
        .map(ModelOptions::from)
        .map_err(|_| protocol(BAD_OPTIONS))?;
    Ok(ContractRequest {
        system_prompt: request.system_prompt,
        history,
        tools,
        options,
    })
}

fn declaration(declaration: Declaration) -> Result<ToolDeclaration, ProviderError> {
    let kind = match declaration.kind {
        DeclarationKind::Function(schema) => p1_contracts::tool::DeclarationKind::Function {
            input_schema: serde_json::from_str(&schema).map_err(|_| protocol(BAD_SCHEMA))?,
        },
        DeclarationKind::Freeform(grammar) => p1_contracts::tool::DeclarationKind::Freeform {
            grammar: grammar.map(|grammar| Grammar {
                syntax: grammar.syntax,
                definition: grammar.definition,
            }),
        },
    };
    Ok(ToolDeclaration {
        name: declaration.name,
        description: declaration.description,
        kind,
    })
}

fn new_parser(composition: &Composition) -> ChatParser {
    ChatParser::new(
        composition.route.origin(&composition.wire_model),
        composition.route.dialect,
    )
}

struct Component;

impl Guest for Component {
    fn configure(settings: ProviderSettings) -> Result<(), String> {
        let composition = settings::compose(
            &settings.origin_route,
            &settings.endpoint,
            &settings.wire_model,
            &settings.adapter_settings,
        )
        .map_err(error_json)?;
        COMPOSITION.with(|cell| {
            cell.set(composition)
                .map_err(|_| error_json(protocol(CONFIGURED_TWICE)))
        })
    }

    fn describe() -> String {
        // No error channel here: before `configure` the answer is empty text, which is not
        // a route description, so the host refuses it as invalid output (`Protocol`).
        with_composition(|composition| {
            let description = composition.route.describe(&composition.wire_model);
            Ok(json(&WireRouteDescription::from(description)))
        })
        .unwrap_or_default()
    }

    fn validate(request: ProviderRequest) -> Result<(), String> {
        with_composition(|composition| {
            let request = decode_request(request)?;
            validate_request(
                &composition.route,
                &composition.wire_model,
                &composition.profile,
                &request,
            )
        })
        .map_err(error_json)
    }

    fn lower(
        request: ProviderRequest,
        _connection: ConnectionState,
    ) -> Result<LoweredRequest, String> {
        with_composition(|composition| {
            let request = decode_request(request)?;
            // `lower_request` validates first (its body builder runs `validate`), exactly as
            // the native provider does before it opens a transport.
            let lowered = lower_request(
                &composition.route,
                &composition.wire_model,
                &composition.profile,
                &request,
            )?;
            let path = if lowered.path.is_empty() {
                settings::http_target(&composition.route.endpoint).1
            } else {
                lowered.path.to_owned()
            };
            Ok(LoweredRequest::Http(HttpRequest {
                method: Method::Post,
                path,
                headers: lowered.headers,
                // An API key or token sent as a bearer; no account header.
                credential: CredentialUse {
                    scheme: CredentialScheme::Bearer,
                    account_id_header: None,
                },
                body: lowered.body,
            }))
        })
        .map_err(error_json)
    }

    fn classify(head: ResponseHead, body: Vec<u8>) -> String {
        let error = with_composition(|composition| {
            Ok(new_parser(composition).on_http_error(head.status, &head.headers, &body))
        })
        .unwrap_or_else(|error| error);
        error_json(error)
    }
}

impl DecodingGuest for Component {
    type Decoder = Decoder;
}

/// One response attempt: a fresh parser, and whether its terminal event was returned.
struct Decoder {
    state: RefCell<DecoderState>,
}

struct DecoderState {
    /// `None` when the decoder was created before `configure`.
    parser: Option<ChatParser>,
    terminated: bool,
}

impl GuestDecoder for Decoder {
    fn new() -> Self {
        let parser = COMPOSITION.with(|cell| cell.get().map(new_parser));
        Self {
            state: RefCell::new(DecoderState {
                parser,
                terminated: false,
            }),
        }
    }

    fn feed(&self, event: WireEvent) -> Vec<String> {
        let mut state = self.state.borrow_mut();
        if state.terminated {
            panic!("{USED_AFTER_TERMINAL}");
        }
        let Some(parser) = state.parser.as_mut() else {
            state.terminated = true;
            let failure = Outcome::Failed(protocol(NOT_CONFIGURED));
            return vec![event_json(StreamEvent::Finished(failure))];
        };
        let mut events = Vec::new();
        let mut terminated = false;
        for event in parser.on_event(SseEvent {
            event: event.name,
            data: event.data,
        }) {
            terminated = matches!(event, StreamEvent::Finished(_));
            events.push(event_json(event));
            // Exactly one `Finished` per decoder: whatever a parser returns after its own
            // terminal event is dropped, as the native drive loop drops it.
            if terminated {
                break;
            }
        }
        state.terminated = terminated;
        events
    }

    fn finish(&self) -> String {
        let mut state = self.state.borrow_mut();
        if state.terminated {
            panic!("{USED_AFTER_TERMINAL}");
        }
        state.terminated = true;
        let outcome = match state.parser.as_mut() {
            Some(parser) => parser.on_end(),
            None => Outcome::Failed(protocol(NOT_CONFIGURED)),
        };
        event_json(StreamEvent::Finished(outcome))
    }

    fn response_id(&self) -> Option<String> {
        // The Chat Completions wire names no response a later request refers to.
        None
    }
}

p1_bindings_provider::generated::export!(Component);

/// The exports driven natively, as the broker drives them: the same Rust functions the
/// component exports. Each test runs on its own thread, so each has a fresh instance.
#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
id        = "deepseek-v4.1-flash"
revision  = 1
model_id  = "deepseek-v4.1-flash"
family    = "deepseek"
thinking  = "enabled"
efforts        = ["high", "max"]
default_effort = "high"
"#;

    /// `routes/opencode-go-subscription.toml` bound to `deepseek-v4.1-flash`.
    fn configure() {
        let adapter_settings = serde_json::json!({
            "dialect": "thinking-with-reasoning-alias",
            "session_header": "x-opencode-session",
            "model_profile": {"stem": "deepseek-v4.1-flash", "toml": PROFILE},
            "route_headers": {},
            "model_binding": {},
        });
        Component::configure(ProviderSettings {
            origin_route: "openai-chat/opencode-go-subscription".into(),
            endpoint: "https://opencode.ai/zen/go/v1/chat/completions".into(),
            model: "deepseek-v4.1-flash".into(),
            wire_model: "deepseek-v4.1-flash".into(),
            adapter_settings: adapter_settings.to_string(),
        })
        .expect("the shipped route configures");
    }

    fn request(history: Vec<String>, options: &str) -> ProviderRequest {
        ProviderRequest {
            system_prompt: "be brief".into(),
            history,
            tools: vec![],
            options: options.into(),
        }
    }

    fn connection() -> ConnectionState {
        ConnectionState {
            open: false,
            last_clean_response: None,
            failed_before_output: false,
        }
    }

    fn error(text: &str) -> WireProviderError {
        serde_json::from_str(text).expect("a provider-error")
    }

    fn event(text: &str) -> WireStreamEvent {
        serde_json::from_str(text).expect("a stream-event")
    }

    fn sse(data: &str) -> WireEvent {
        WireEvent {
            name: None,
            data: data.into(),
        }
    }

    #[test]
    fn every_export_before_configure_is_a_protocol_error() {
        let refused = Component::validate(request(vec![], "{}")).unwrap_err();
        assert_eq!(error(&refused).message, NOT_CONFIGURED);
        assert_eq!(Component::describe(), "");
    }

    #[test]
    fn lower_puts_the_endpoints_last_segment_in_the_path_and_no_credential() {
        configure();
        let user = r#"{"item":"user","text":"hello"}"#.to_owned();
        let LoweredRequest::Http(http) = Component::lower(
            request(vec![user], r#"{"cache_key":"session-1"}"#),
            connection(),
        )
        .expect("the request lowers") else {
            panic!("a chat route lowers to HTTP");
        };
        assert_eq!(http.path, "/completions");
        assert!(
            http.headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
        );
        assert!(
            http.headers
                .contains(&("x-opencode-session".to_owned(), "session-1".to_owned()))
        );
        assert!(matches!(http.credential.scheme, CredentialScheme::Bearer));
        assert_eq!(http.credential.account_id_header, None);
    }

    #[test]
    fn a_request_the_route_cannot_carry_is_refused_before_lowering() {
        configure();
        let mut freeform = request(vec![], "{}");
        freeform.tools = vec![Declaration {
            name: "apply_patch".into(),
            description: "edit".into(),
            kind: DeclarationKind::Freeform(None),
        }];
        let refused = error(&Component::lower(freeform, connection()).unwrap_err());
        assert_eq!(
            ProviderErrorKind::from(refused.kind),
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn a_decoder_ends_with_exactly_one_finished() {
        configure();
        let decoder = Decoder::new();
        let wire = [
            r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ];
        let events: Vec<WireStreamEvent> = wire
            .into_iter()
            .flat_map(|data| decoder.feed(sse(data)))
            .map(|text| event(&text))
            .collect();
        let finished = events
            .iter()
            .filter(|event| matches!(event, WireStreamEvent::Finished { .. }))
            .count();
        assert_eq!(finished, 1, "{events:?}");
        assert!(matches!(events[0], WireStreamEvent::TextDelta { .. }));
        assert!(matches!(
            events.last(),
            Some(WireStreamEvent::Finished { .. })
        ));
        assert_eq!(decoder.response_id(), None);
    }

    #[test]
    #[should_panic(expected = "decoder used after its terminal event")]
    fn a_decoder_fed_after_its_terminal_event_traps() {
        configure();
        let decoder = Decoder::new();
        decoder.finish();
        decoder.feed(sse("[DONE]"));
    }

    #[test]
    fn classify_never_copies_the_body() {
        configure();
        let head = ResponseHead {
            status: 400,
            headers: vec![],
        };
        let body = br#"{"error":{"message":"SECRET-BODY-TEXT"}}"#;
        let classified = error(&Component::classify(head, body.to_vec()));
        assert!(!classified.message.contains("SECRET-BODY-TEXT"));
    }
}
