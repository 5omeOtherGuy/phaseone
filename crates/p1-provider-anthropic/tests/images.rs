//! Attachment blocks on the Messages wire (ADR-0180): an image becomes a base64 image block
//! when the profile takes images, its placeholder when it does not, and a file always
//! becomes the one line naming its read-only path.

use p1_contracts::{
    AttachmentId, FileRef, ImageData, ImageRef, ImageType, InputModality, Item, ModelOptions,
    ProviderErrorKind, ProviderRequest, UserBlock,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{
    MessagesAccount, MessagesRoute, ROUTE, build_request, validate_request,
};
use serde_json::{Value, json};

const ID: &str = "abababababababababababababababababababababababababababababababab";

fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
        long_context: false,
    }
}

fn profile(input_modalities: Vec<InputModality>) -> ModelProfile {
    ModelProfile {
        id: "claude-example".into(),
        revision: 1,
        model_id: "claude-example".into(),
        family: "claude".into(),
        thinking: ThinkingPolicy::EffortLevel,
        efforts: Vec::new(),
        default_effort: None,
        thinking_budgets: Default::default(),
        context_tokens: None,
        max_output_tokens: Some(1_000),
        input_modalities,
    }
}

fn image() -> ImageRef {
    ImageRef {
        id: AttachmentId::new(ID).unwrap(),
        media_type: ImageType::Png,
        bytes: 3,
        width: 4,
        height: 5,
    }
}

fn request(data: Option<ImageData>, path: Option<&str>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: String::new(),
        history: vec![Item::UserContent {
            blocks: vec![
                UserBlock::text("what is this?"),
                UserBlock::Image {
                    image: image(),
                    data,
                },
                UserBlock::File {
                    file: FileRef {
                        id: AttachmentId::new(ID).unwrap(),
                        name: "notes.txt".into(),
                        bytes: 12,
                    },
                    path: path.map(str::to_owned),
                },
            ],
        }],
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

fn content(body: &Value) -> &Value {
    &body["messages"][0]["content"]
}

#[test]
fn an_image_model_gets_a_base64_image_block() {
    let request = request(Some(ImageData::encode(b"abc")), Some("/s/objects/x"));
    let profile = profile(vec![InputModality::Text, InputModality::Image]);
    let body = build_request(&route(), "claude-example", &profile, &request).unwrap();
    assert_eq!(
        content(&body),
        &json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                "data": "YWJj"}},
            {"type": "text", "text":
                "[attached file \"notes.txt\", 12 bytes, read-only at /s/objects/x]",
                "cache_control": {"type": "ephemeral"}},
        ])
    );
    assert_eq!(
        route()
            .describe_model("claude-example", &profile)
            .input_modalities,
        vec![InputModality::Text, InputModality::Image]
    );
}

#[test]
fn a_text_only_model_gets_the_placeholder_and_never_the_bytes() {
    let request = request(Some(ImageData::encode(b"abc")), Some("/s/objects/x"));
    let body = build_request(&route(), "claude-example", &profile(Vec::new()), &request).unwrap();
    let blocks = content(&body).as_array().unwrap();
    assert_eq!(blocks[1]["type"], "text");
    assert_eq!(
        blocks[1]["text"],
        format!(
            "[image {}: image/png 4x5, 3 bytes; this model does not take image input]",
            &ID[..12]
        )
    );
    assert!(!body.to_string().contains("YWJj"));
    assert!(
        route()
            .describe_model("claude-example", &profile(Vec::new()))
            .input_modalities
            .is_empty()
    );
}

#[test]
fn an_unresolved_image_validates_but_is_never_sent_without_its_bytes() {
    let request = request(None, None);
    let profile = profile(vec![InputModality::Image]);
    validate_request(&route(), "claude-example", &profile, &request).unwrap();
    let error = build_request(&route(), "claude-example", &profile, &request).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("not resolved"), "{}", error.message);
}
