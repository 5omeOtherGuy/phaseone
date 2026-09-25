# Provider split — design note (oracle consultation, 2026-09-20)

Read-only consultation by gpt-6-astra (high) on the question in `../phaseone-briefs/oracle-provider-split.md`.
ADOPTED by the owner on 2026-09-20 (ADR-0039); where this note and an ADR differ, the ADR wins.
File paths in it refer to the state of main and of the unmerged `task/9-subscription-routes` at that time.

---

Keep **three independent inputs—wire adapter, route/account, and model profile—but only one runtime `Provider`**. Make profiles reusable data with a small set of compiled behavior strategies; keep prompts and exact tools in environments. Reshape the unmerged chat adapter before merging: its route enum currently selects model behavior, which is precisely the coupling you need to remove.

The axes are independent selections, **not a promise that every combination works**. “Same model behavior across routes” should mean identical model policy wherever faithfully expressible, and an assembly error otherwise—not identical JSON, identical outputs, or portable reasoning signatures.

### 1. The boundary to establish

The existing runtime seam is already appropriate: [`Provider`](crates/p1-contracts/src/provider.rs) represents a concrete model plus route, validates requests, and produces the common stream. Keep it.

Use these ownership rules:

| Component | Owns |
|---|---|
| **Model profile** | Model identity, supported/default effort and its interpretation, reasoning continuation requirements, model limits, preferred cache policy, supported tool forms |
| **Wire adapter** | Encoding those policies into a supported protocol/dialect; message layout, tool serialization, stream parsing, completion boundaries, usage decoding, opaque replay codecs |
| **Route/account** | Endpoint, authentication source, mandatory headers/prompt wrapping, subscription restrictions, supported protocol extensions, model-name mapping, route limits, reporting/billing policy |
| **Environment** | Whole prompt, exact tools and faces, explicit options, context-management policy |
| **Assembly** | Resolve the combination, reject incompatibilities, record the effective configuration |

A two-axis implementation that keeps profiles inside adapters still duplicates model decisions across routes. Putting all profile fields directly into each environment removes that coupling but introduces another: every prompt/tool variant must repeat the model’s continuation and effort rules. **Reference one named profile from many environments.** A profile does not need a trait or its own runtime object.

Concretely, move today’s behavior as follows:

| Existing behavior | Destination |
|---|---|
| Anthropic `is_adaptive`, manual effort budgets, default output allowance | Profile policy. Replace model-prefix guessing with explicit model records. |
| Anthropic adaptive/manual JSON, message coalescing, function declarations | Messages adapter. |
| Anthropic cache breakpoint selection | Profile cache policy; Messages adapter locates the corresponding wire blocks and writes `cache_control`. Route constrains availability/TTL. |
| Claude Code identity prefix, OAuth/beta requirements, endpoint | Claude subscription route configuration/implementation. A Claude profile must not force the Claude Code identity on another route. |
| Anthropic signatures and redacted-thinking payloads | Messages replay codec; profile specifies continuation requirements, never interprets signatures. |
| GPT supported effort levels and default verbosity | Profile defaults/support, intersected with verified route restrictions. Preserve current restrictions until evidence establishes whether they are model-wide or Codex-specific. |
| Responses reasoning fields, encrypted replay, custom/function tool encoding | Responses adapter. |
| Codex `store:false`, rejected output-cap parameter, account header, session/conversation headers | Codex route policy. |
| DeepSeek effort restrictions and enabled thinking | DeepSeek profile; chat adapter translates them. |
| GLM effort/output limits and retained-thinking policy | GLM profile, with any lower endpoint limit in the route binding. |
| GLM `clear_thinking:false` and `tool_stream:true` | Typed chat encodings of the GLM behavior, enabled only on bindings known to support them. Do not turn them into generic properties of all Chat Completions endpoints. |
| Go endpoint, `x-opencode-session`, credential lookup | Route/account. |
| Chat `reasoning` versus `reasoning_content`, usage-field variants | Explicit chat dialect handling. A display field is replayable only when that dialect’s continuation contract establishes it. |
| Context window | Model ceiling constrained by route/model binding; environment chooses summarization thresholds and may choose a smaller working window. |

These distinctions come directly from [`Anthropic request construction`](crates/p1-provider-anthropic/src/request.rs:41), [`Codex request construction`](crates/p1-provider-openai/src/request.rs:54), and the unmerged [`chat validation/build_request`](crates/p1-provider-openai-chat/src/request.rs:13).

Two existing behaviors should not become architectural precedents:

- Anthropic silently raises an explicitly supplied output cap when the thinking budget exceeds it. Reject that conflict; only adjust an unspecified default.
- The chat parser currently turns every recognized reasoning string into replay data. That is valid only for its supported continuation dialects, not for arbitrary aggregators.

### 2. The smallest concrete design

Use plain structs and ordinary constructors. No `Route` trait, `ModelProfile` trait, universal request intermediate representation, or new dispatch framework. (Dated note: the compile-time wiring below is superseded by ADR-0070, which makes providers WebAssembly modules the host loads by name; the route × profile × adapter split itself stands.)

A small shared **`p1-model-profile`** crate is justified because assembly and multiple adapters consume the same model policy. It depends on contracts, not providers, transport, or tools. This is the one new architectural crate I would propose; it requires the repository’s normal dependency/ADR decision.

Representative shapes—not a complete public API:

```rust
// p1-model-profile
pub struct ModelProfile {
    pub id: String,
    pub revision: u32,
    pub model_id: String,       // canonical configured identity
    pub family: String,
    pub behavior: ModelBehavior,
    pub efforts: Vec<Effort>,
    pub default_effort: Option<Effort>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub tool_forms: ToolForms,
    pub cache: CachePolicy,
}

pub enum ModelBehavior {
    ClaudeAdaptive,
    ClaudeBudget,
    GptReasoning,
    DeepSeekThinking,
    GlmRetainedThinking,
}
```

The behavior enum selects implemented semantics, **not routes**. Share pure effort/budget policy here; keep protocol field names and replay codecs in adapters. Add strategies only when an actual model requires different behavior.

Route files belong to the host’s configuration layer:

```rust
// p1-host::routes; configuration, never credential values
pub struct RouteFile {
    pub id: String,             // stable route/account identity
    pub adapter: String,        // compiled constructor key
    pub endpoint: String,
    pub credential: CredentialRef,
    pub headers: BTreeMap<String, String>, // non-secret only
    pub models: BTreeMap<String, ModelBinding>,
}

pub struct ModelBinding {
    pub wire_model: String,
    pub context_limit: Option<u64>,
    pub output_limit: Option<u32>,
    // Typed adapter settings: supported extensions and restrictions.
}
```

Deserialize adapter settings into a typed struct owned by that adapter. For example:

```rust
// p1-provider-openai-chat
pub struct ChatRoute {
    pub origin_route: String,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub session_header: Option<String>,
    pub dialect: ChatDialect,
    pub limits: ChatLimits,
}

impl ChatProvider {
    pub fn new(
        route: ChatRoute,
        wire_model: String,
        profile: Arc<ModelProfile>,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, ProviderError>;
}
```

`ChatDialect` is a finite set of **implemented wire behaviors**, not `OpenCodeGo | Glm | OpenRouter`. It governs reasoning fields/replay, supported request extensions, and usage semantics. Use typed settings rather than arbitrary JSON patches.

The constructor must reject unsupported profile/dialect combinations. Its request builder then uses the same resolved behavior for validation and serialization.

**Reuse the current catalog.** Extend [`ProviderSpec`](crates/p1-assembly/src/lib.rs:130) to carry the selected profile. The host reads route records and registers an existing constructor closure for each route. Each closure captures typed route data, model bindings, and the credential source.

That was compile-time composition as of this note: configuration supplies constructor arguments; only implementations explicitly wired in [`register_providers`](crates/p1-host/src/catalog.rs:190) can execute. Superseded by ADR-0070 (2026-09-25): the same keys will name WebAssembly modules; the rule that only an assembled provider can execute stands.

An environment can become:

```toml
route   = "opencode-go-subscription"
profile = "deepseek-v4.1-flash"

[options]
reasoning_effort = "high"

[[tools]]
module = "read"
[[tools]]
module = "edit"
[[tools]]
module = "write"
[[tools]]
module = "grep"
[[tools]]
module = "shell"
[[tools]]
module = "finish"
```

Keep `prompt.md` unchanged. Derive `family` and canonical model identity from the profile. Resolve the endpoint’s model spelling through the route’s model binding. Changing the route therefore preserves the selected profile and environment.

For a genuinely compatible new endpoint, adding its route record, credential reference, and model-name bindings requires **no Rust change**. A route requiring an unimplemented reasoning extension needs code for that extension. Calling something “OpenAI-compatible” cannot make that limitation disappear.

**Assembly must resolve requirements, not merely consult booleans.** Extend the existing [`assemble`](crates/p1-assembly/src/lib.rs:466) sequence to:

1. Resolve profile, route, model binding, and compiled adapter.
2. Validate the profile’s required reasoning/continuation behavior against the binding.
3. Build the exact tools and prompt.
4. Validate actual declarations, explicit options, output/context limits, and native options using the same lowering logic used by the request builder.
5. Produce the effective description and configuration.

`RouteDescription` should describe this **resolved provider instance**. Its freeform support is the intersection of model, protocol implementation, and route—not a protocol-wide assertion.

Add a small explicit cache-key policy to the description. Replace [`assemble_with_cache_key`](crates/p1-host/src/run.rs:930), which currently retries assembly without a generated key after *any* assembly error. Generate a key only when supported; build tools once. Explicit unsupported keys remain errors.

For native options, reject unconsumed options during assembly. The current “ignore another namespace” rule can silently lose an explicit preference when switching routes.

**Credentials remain outside profiles and manifests.** Implement the report’s env → p1 store → external CLI precedence in a host auth module initially, behind existing `CredentialSource`. Extract `p1-auth` when implementing the shared store, without coupling that work to the first chat merge. Preserve source provenance so OAuth refresh writes back to the selected source, never a different fallback. Reuse the existing non-blocking lock and atomic-write behavior.

### 3. Origin and replay: preserve the conservative rule

Keep [`Origin`](crates/p1-contracts/src/history.rs:12) as:

```text
origin.route = stable configured route/account identity
origin.model = configured wire model identifier
```

Preserve existing origin strings during migration. Correct the stale `Origin.model` comment: implementations and ADR-0018 use the configured model, not the response’s echoed model.

A profile’s canonical model identity is **not** a replay compatibility key. The same DeepSeek profile through Go and another route should have the same policy but different origins.

Keep [ADR-0033](docs/adr/0033-a-session-resumes-only-on-the-route-and-model-that-recorded-it.md): cross-route resume remains rejected. This architecture change does not justify relaxing it.

Replay compatibility should additionally identify the codec:

```text
exact origin + replay codec identifier + payload version
```

You can retain the current `ReplayData` shape: introduce a versioned payload envelope containing the codec identifier when multiple layouts become possible within a route. Explicitly support existing version-1 payloads on their existing adapters. No journal-wide rewrite is necessary.

Profile revision belongs in the resolved manifest for reproducibility; it is not automatically a replay-format revision. Reject unsupported **same-origin** replay rather than silently dropping it. Preserve opaque payloads separately from display text.

A route ID must not silently acquire a new endpoint/account/protocol meaning. Treat those changes as a new route identity or binding revision reflected in that identity; credential rotation alone does not change it. Otherwise configurable routes undermine ADR-0033 despite unchanged Rust equality checks.

### 4. Keep one conformance suite, with different evidence at each boundary

Continue running [`run_all`](crates/p1-provider-conformance/src/lib.rs:801) against **composed providers**. `RouteUnderTest` already has the constructor and fixtures needed; no second conformance framework is necessary.

| Boundary | Required checks |
|---|---|
| **Wire adapter/dialect** | Shared terminal, ordering, truncation, invalid arguments, cancellation, retry, usage-absence, and replay checks. Exercise each materially distinct parser/replay dialect. |
| **Route** | Run the same suite for shipped route configurations; add focused assertions for endpoint, headers, wrapping, restrictions, reporting, and injected credential resolution. |
| **Profile** | Pure effort/default/limit tests; golden requests for each supported encoding; negative assembly tests for unsupported encodings. |
| **Cross-route model behavior** | Bind one profile to two synthetic routes. Assert identical model policy and model-related body fields where the dialect is the same; only declared route differences may change. |
| **Environment** | Existing prompt/tool coherence checks, plus profile/route/tool compatibility and effective context limits. |

Do not require every possible Cartesian product. Cover shipped combinations and every distinct implemented behavior.

Strengthen [`reasoning_replay_round_trips`](crates/p1-provider-conformance/src/lib.rs:329): today it checks whether payload string leaves occur somewhere in serialized JSON. Keep that frozen check and add exact structural assertions for the reasoning item/message location, plus a real second request through the scripted transport. Substring presence does not prove correct replay placement.

Offline fixtures establish implementation behavior, not endpoint support. Mark route/model extension claims as verified or provisional and validate new combinations through the existing lead-only live-check workflow. Do not add live network to conformance.

### 5. Migration order and scope

**Reshape the chat work before merging; do not merge the closed route enum as the intended API.** Retain its parser, HTTP driver integration, fixtures, and shared-suite coverage.

1. **Record the boundary decision and characterization tests — M, 1–3h.**  
   Add an ADR describing the three inputs and unchanged runtime seam. Capture current first-party request/replay behavior. Preserve ADR-0033.

2. **Change the unmerged chat constructor — M/L, roughly ½–1 day.**  
   Replace `SubscriptionRoute` with injected route data; supply model behavior separately; move env/file lookup decisions to host auth wiring. Keep the two existing catalog keys and environment behavior. The shared profile shape can land additively first. This does not require completing the p1 credential store.

3. **Add profile selection and data-driven chat routes — L, 1–2 days.**  
   Extend `ProviderSpec`, environment parsing, route/model mappings, and assembly validation. Support old `provider/model/family` files temporarily through explicit compatibility mappings for shipped configurations; reject unknown mappings instead of guessing. Add the synthetic two-route/one-profile test.

4. **Extract first-party policy incrementally — L, 1–2 days.**  
   Move Claude model classification/budgets and GPT model defaults into profiles. Inject route identity into parsers/builders. Keep current protocol implementations and first-party credentials working throughout.

5. **Complete auth storage and remove compatibility scaffolding separately.**  
   Storage is its own change with precedence, rotation, contention, permissions, and atomic-write tests.

Each increment should independently pass the gate before merge, then the exact-commit CI check. This is **several days of bounded work**, not a single broad refactor.

The first-party adapters should **not** become a universal provider engine, a stack of profile middleware, or implementations of a new generic `WireProtocol` trait. Their existing request/parser organization plus shared [`drive`](crates/p1-provider-http/src/drive.rs:56) already provides the useful reuse. Leave specialized subscription construction intact until a second real Messages/Responses route needs further extraction.

### 6. Main risks and guardrails

| Risk | Guardrail |
|---|---|
| **Capability declarations overpromise** | Constructor/lowering validation is authoritative. Claims require fixtures and route evidence; descriptive flags cannot enable missing encoders. |
| **Profile explosion** | Separate model records from shared behavior strategies. No per-route copies of entire profiles; no inheritance framework initially. |
| **A quirk is really route × model** | Put a narrow typed restriction/encoding choice on `ModelBinding`, with evidence and a test. It may restrict support or select an equivalent encoding, not silently change requested semantics. |
| **Aggregator hides or rewrites continuation data** | Require an implemented dialect that preserves the necessary opaque material; otherwise reject that profile/binding. Never manufacture replay from a summary. |
| **“Portable” profile erases native strengths** | Validate the environment’s actual freeform/function declarations. Unsupported native tools fail assembly; any alternative tool face must be explicitly selected in an environment. |
| **Route configuration leaks credentials** | Route data contains credential references only; reject secret-bearing static headers/URLs. Retain redacted credential objects and keep secrets out of resolved manifests. |
| **Config changes bypass resume safety** | Stable route/account identity, explicit binding revisions, codec-version checks, and unchanged cross-origin rejection. |

Consider a trait-based profile extension or more general protocol abstraction only when multiple real models cannot be represented cleanly by these small strategies, or multiple working adapters duplicate substantial lowering logic. Consider cross-route continuation only as a separate feature with explicit history translation and tests; it is not needed to make model behavior consistent across newly started sessions.

This was a read-only source review; I made no file changes and did not run tests or live provider calls.