//! The routes and model profiles the component suites compose their providers from. They
//! are the core API-key routes (ADR-0197) and the subscription connectors' files as shipped
//! on 2026-10-11, carried here with the account references replaced by inline credential
//! tables, so that a moved or edited catalog file cannot break a suite (#843). A suite
//! writes them into a [`CatalogFixture`] and loads them through the host's route loader
//! like any catalog; the endpoints, dialects and wire models are the protocol inputs its
//! assertions pin.

use p1_testkit::catalog::CatalogFixture;

/// `routes/anthropic.toml`: the pay-as-you-go Messages API with the key in `x-api-key`.
pub const ANTHROPIC_API_KEY: &str = r#"id           = "anthropic"
origin_route = "anthropic-messages/anthropic"
adapter      = "anthropic-messages"
endpoint     = "https://api.anthropic.com"

[credential]
kind       = "api-key"
env        = "ANTHROPIC_API_KEY"
store_only = true

[adapter_settings]
dialect           = "api-key"
credential_header = "x-api-key"

[models."claude-sonnet-5"]
wire_model = "claude-sonnet-5"
"#;

/// `routes/openai.toml`: the pay-as-you-go Responses API with a bearer key.
pub const OPENAI_API_KEY: &str = r#"id           = "openai"
origin_route = "openai-responses/openai"
adapter      = "openai-responses"
endpoint     = "https://api.openai.com"

[credential]
kind       = "api-key"
env        = "OPENAI_API_KEY"
store_only = true

[adapter_settings]
dialect = "api-key"

[models."gpt-6-sol"]
wire_model = "gpt-6-sol"
"#;

/// `routes/anthropic-subscription.toml`: the Claude Code subscription over the Messages
/// adapter with the 1M-token context window requested.
pub const ANTHROPIC_SUBSCRIPTION: &str = r#"id           = "anthropic-subscription"
origin_route = "anthropic-messages/claude-subscription"
adapter      = "anthropic-messages"
endpoint     = "https://api.anthropic.com"

[credential]
kind       = "claude-code-oauth"
store_only = true

[adapter_settings]
dialect = "claude-code-subscription"
long_context = true

[models."claude-sonnet-5"]
wire_model = "claude-sonnet-5"
"#;

/// `routes/openai-codex-subscription.toml`: the ChatGPT/Codex subscription over the
/// Responses adapter, WebSocket first.
pub const OPENAI_CODEX_SUBSCRIPTION: &str = r#"id           = "openai-codex-subscription"
origin_route = "openai-responses/codex-subscription"
adapter      = "openai-responses"
endpoint     = "https://chatgpt.com/backend-api"

[credential]
kind       = "codex-oauth"
store_only = true

[adapter_settings]
dialect = "codex-subscription"
transport = "websocket"

[models."gpt-5.6-sol"]
wire_model = "gpt-5.6-sol"
"#;

/// `routes/glm-subscription.toml`: the Z.ai coding plan over the Chat adapter's
/// retained-thinking dialect, with no session header.
pub const GLM_SUBSCRIPTION: &str = r#"id           = "glm-subscription"
origin_route = "openai-chat/glm-subscription"
adapter      = "openai-chat"
endpoint     = "https://api.z.ai/api/coding/paas/v4/chat/completions"
retry_policy = "patient"

[credential]
kind       = "api-key"
env        = "ZAI_API_KEY"
store_only = true

[adapter_settings]
dialect = "retained-thinking"

[models."glm-5.3"]
wire_model = "glm-5.3"

[models."glm-5.3-flash"]
wire_model = "glm-5.3-flash"
"#;

/// `routes/opencode-go-subscription.toml`: OpenCode Go over the Chat adapter's
/// thinking-with-reasoning-alias dialect with a session header.
pub const OPENCODE_GO_SUBSCRIPTION: &str = r#"id           = "opencode-go-subscription"
origin_route = "openai-chat/opencode-go-subscription"
adapter      = "openai-chat"
endpoint     = "https://opencode.ai/zen/go/v1/chat/completions"
retry_policy = "deepseek"

[credential]
kind       = "api-key"
env        = "OPENCODE_API_KEY"
store_only = true

[adapter_settings]
dialect        = "thinking-with-reasoning-alias"
session_header = "x-opencode-session"

[models."deepseek-v4.1-flash"]
wire_model = "deepseek-v4.1-flash"
"#;

/// `routes/glm-messages.toml`: Z.ai's native Messages wire (ADR-0147).
pub const GLM_MESSAGES: &str = r#"id           = "glm-messages"
origin_route = "anthropic-messages/glm-messages"
adapter      = "anthropic-messages"
endpoint     = "https://api.z.ai/api/anthropic"

[credential]
kind       = "api-key"
env        = "ZAI_API_KEY"
store_only = true

[adapter_settings]
dialect = "zai"
credential_header = "x-api-key"

[models."glm-5.3"]
wire_model = "glm-5.3"

[models."glm-5.3-flash"]
wire_model = "glm-5.3-flash"
"#;

/// `routes/opencode-go-messages.toml`: OpenCode Go's Messages wire (ADR-0138).
pub const OPENCODE_GO_MESSAGES: &str = r#"id = "opencode-go-messages"
origin_route = "anthropic-messages/opencode-go-messages"
adapter = "anthropic-messages"
endpoint = "https://opencode.ai/zen/go"
retry_policy = "deepseek"

[credential]
kind       = "api-key"
env        = "OPENCODE_API_KEY"
store_only = true

[adapter_settings]
dialect = "opencode-go"
credential_header = "x-api-key"

[models."deepseek-v4.1-flash"]
wire_model = "deepseek-v4.1-flash"
"#;

/// `profiles/claude-sonnet-5.toml`: effort-level thinking, no default effort, images.
pub const CLAUDE_SONNET_5: &str = r#"id       = "claude-sonnet-5"
revision = 2
model_id = "claude-sonnet-5"
family   = "claude"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
input_modalities = ["text", "image"]
"#;

/// `profiles/gpt-5.6-sol.toml`: effort-level reasoning, no default effort.
pub const GPT_5_6_SOL: &str = r#"id       = "gpt-5.6-sol"
revision = 2
model_id = "gpt-5.6-sol"
family   = "gpt"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
"#;

/// `profiles/gpt-6-sol.toml`: effort-level reasoning, no default effort.
pub const GPT_6_SOL: &str = r#"id       = "gpt-6-sol"
revision = 1
model_id = "gpt-6-sol"
family   = "gpt"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
"#;

/// `profiles/glm-5.3.toml`: preserved thinking with a default effort and an output ceiling.
pub const GLM_5_3: &str = r#"id        = "glm-5.3"
revision  = 1
model_id  = "glm-5.3"
family    = "glm"
thinking  = "preserved"

efforts        = ["low", "high", "max"]
default_effort = "high"

max_output_tokens = 131072
"#;

/// `profiles/glm-5.3-flash.toml`: enabled thinking with a default effort and both capacities.
pub const GLM_5_3_FLASH: &str = r#"id        = "glm-5.3-flash"
revision  = 1
model_id  = "glm-5.3-flash"
family    = "glm"
thinking  = "enabled"

efforts        = ["low", "high", "max"]
default_effort = "high"
context_tokens = 1000000
max_output_tokens = 131072
"#;

/// `profiles/deepseek-v4.1-flash.toml`: enabled thinking with a default effort and both
/// capacities (ADR-0136).
pub const DEEPSEEK_V4_1_FLASH: &str = r#"id        = "deepseek-v4.1-flash"
revision  = 2
model_id  = "deepseek-v4.1-flash"
family    = "deepseek"
thinking  = "enabled"

efforts        = ["low", "high", "max"]
default_effort = "high"

context_tokens = 1000000
max_output_tokens = 384000
"#;

/// The route files above by id, and the profiles by id.
pub const ROUTES: &[(&str, &str)] = &[
    ("anthropic", ANTHROPIC_API_KEY),
    ("openai", OPENAI_API_KEY),
    ("anthropic-subscription", ANTHROPIC_SUBSCRIPTION),
    ("openai-codex-subscription", OPENAI_CODEX_SUBSCRIPTION),
    ("glm-subscription", GLM_SUBSCRIPTION),
    ("opencode-go-subscription", OPENCODE_GO_SUBSCRIPTION),
    ("glm-messages", GLM_MESSAGES),
    ("opencode-go-messages", OPENCODE_GO_MESSAGES),
];

pub const PROFILES: &[(&str, &str)] = &[
    ("claude-sonnet-5", CLAUDE_SONNET_5),
    ("gpt-5.6-sol", GPT_5_6_SOL),
    ("gpt-6-sol", GPT_6_SOL),
    ("glm-5.3", GLM_5_3),
    ("glm-5.3-flash", GLM_5_3_FLASH),
    ("deepseek-v4.1-flash", DEEPSEEK_V4_1_FLASH),
];

/// A temporary catalog holding every route and profile above.
pub fn route_catalog() -> CatalogFixture {
    let fixture = CatalogFixture::new();
    for (id, text) in ROUTES {
        fixture.write_route(id, text);
    }
    for (id, text) in PROFILES {
        fixture.write_profile(id, text);
    }
    fixture
}

/// The profile text of `id`, as [`route_catalog`] writes it.
pub fn profile_text(id: &str) -> &'static str {
    PROFILES
        .iter()
        .find(|(profile, _)| *profile == id)
        .map(|(_, text)| *text)
        .unwrap_or_else(|| panic!("no profile fixture {id}"))
}
