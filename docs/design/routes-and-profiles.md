# Routes and profiles — selecting what a provider is composed from

Status: spec for step 3 of ADR-0039 (profile selection, data-driven routes, assembly
validation). Step 2 (the chat adapter takes injected route data and a profile) is its
precondition; step 4 (the first-party adapters give up their model policy) follows it.
Background and the ownership table: `notes/2026-09-20-provider-split.md`.

## 1. Three kinds of file

```
environments/<name>/environment.toml   prompt + exact tools + options      (exists)
profiles/<id>.toml                     what a MODEL is                     (new)
routes/<id>.toml                       how an ACCOUNT/ENDPOINT is reached  (new)
```

All three are data. None of them can name code that is not compiled in: a route names an
ADAPTER KEY registered in `p1-host::catalog`, a profile names a compiled policy variant (`ThinkingPolicy` today), and an
unknown value of either is a load error that lists the known ones.

Lookup order for `profiles/`, `routes/` and `accounts/` is the one environments already use:
`<dir>/../<kind>` for each environments directory, highest priority first (the user config
directory before the shipped one). The first file for an id wins whole; files never merge
(ADR-0139 §4). `p1 env show` prints a `source` line per definition, marking a file that
shadows a later one of the same id, and `p1 models` marks such rows `shadows:<kinds>`.

### 1.1 Profile file

```toml
id        = "deepseek-v4.1-flash"     # must equal the file stem
revision  = 1
model_id  = "deepseek-v4.1-flash"     # canonical identity, NOT a wire name
family    = "deepseek"
thinking  = "enabled"                 # ThinkingPolicy: enabled | preserved
efforts        = ["high", "max"]
default_effort = "high"               # optional; must be one of `efforts`
context_tokens    = 128000            # optional = unknown
max_output_tokens = 32000             # optional = unknown
```

`p1-model-profile` owns the struct, its `Deserialize`, and `validate()`. Unknown keys are
rejected (`deny_unknown_fields`). `thinking` is the compiled-strategy selector step 2
introduced; it grows into the ADR's `ModelBehavior` when step 4 brings models whose difference
is more than their thinking policy — not before. Fields the split note lists but nothing consumes yet
(`tool_forms`, `cache`) are NOT added in this step — a field arrives with its first consumer.

### 1.2 Route file

```toml
id       = "opencode-go-subscription"  # must equal the file stem; the key environments name
origin_route = "openai-chat/opencode-go-subscription"  # Origin.route, explicit so it never drifts
adapter  = "openai-chat"               # catalog adapter key
endpoint = "https://…/v1"
retry_policy = "deepseek"              # optional: default | deepseek (ADR-0137)

[credential]                           # a REFERENCE, never a value
kind   = "api-key"
env    = "OPENCODE_API_KEY"
borrow = ["opencode:opencode-go", "pi:opencode-go"]   # tried in this order, after env
# store_only = true                    # ADR-0061: env + p1's store only; no CLI login is read
# login_dir = "~/.claude-2"            # ADR-0074: claude-code-oauth only — the Claude Code dir to borrow

[headers]                              # non-secret, static
# name = "value"

[adapter_settings]                     # typed by the adapter named above
dialect        = "thinking-with-reasoning-alias"   # a ChatDialect variant
session_header = "x-opencode-session"
client_identity = "opencode"           # optional; a non-secret client identity (see below)

[models."deepseek-v4.1-flash"]         # key = profile id
wire_model    = "deepseek-v4.1-flash"
context_limit = 128000                 # optional; lowers the profile's ceiling, never raises it
output_limit  = 32000                  # optional; same rule
```

- `retry_policy` selects the native broker's policy for the whole route, not a model
  or component setting. Omission/`default` preserves `RetryPolicy::default`; `deepseek`
  uses five retries, 500 ms doubling, 10 s cap, ±10% multiplicative jitter and exact
  Retry-After up to 10 s. Longer hints surface the original error without waiting.
  Shipped OpenCode Go and ClinePass routes select it; Cline's GLM binding shares it.
- `credential.kind` is a closed enum: `api-key` (above), `claude-code-oauth`, `codex-oauth`,
  `none`. `none` (issue #134) is the route that sends NO credential: an egress proxy injects the
  provider's credential after the request leaves the process, so nothing is read and the adapter
  sends no authentication header (`docs/design/credentials.md` §9). It names no source, so `env`,
  a nonempty `borrow` and `store_only` beside it are load errors.
  `store_only` (default `false`, ADR-0061) is the one policy field: written, the chain is the
  documented variable and p1's own store, and no other tool's login file is read for ANY kind
  (for `claude-code-oauth` / `codex-oauth` that is what removes the unconditional CLI fallback;
  for `api-key` it is the same statement as `borrow = []`, and combining it with a non-empty
  `borrow` is a load error). Every shipped route sets it, so p1 is self-contained at runtime
  (`docs/design/credentials.md` §8) — except `anthropic-subscription-2` (below).
  `login_dir` (ADR-0074) is allowed ONLY on `claude-code-oauth`: the Claude Code config directory
  whose login the route borrows (absolute, or `~/…` expanded against the home directory; absent →
  `$CLAUDE_CONFIG_DIR`, else `~/.claude`). On any other kind it is a load error
  (`docs/design/credentials.md` §10).
  A header whose name is `authorization`, `x-api-key`, `cookie` or starts with `x-auth` is
  rejected in `[headers]`: a route file must not be able to hold a secret by accident.
- `[adapter_settings]` is deserialized by the adapter's own typed struct with
  `deny_unknown_fields` — in the provider component that serves a request, and when a route file
  loads, against the host's copy of that struct (S7.10-R4: the native adapters are not in the
  host's dependency graph; same type names, fields, defaults and serde attributes, so a malformed
  table fails at the same stage with the same message). The host hands the component the table as
  a `toml::Value`, adding only its own reserved keys.
- `[adapter_settings] client_identity` (optional, `openai-chat` only) makes a route present
  a vendor's own client identity to a gateway that gates a free tier on it. The only value is
  `opencode`: the request carries OpenCode's `user-agent` and `x-opencode-*` headers and a
  `ses_`/`msg_` id derived from the route's cache key. It declares no tool of its own; the
  gate's `bash` and `read` names are an environment's business (`[[tools]] name = "bash"` /
  `"read"`), so the system prompt and p1's own tools are unchanged. Only the shipped Zen free
  routes set it (owner decision 2026-09-24, ADR-0067; evidence and the minimal accepted shape
  in `docs/design/zen-client-identity-evidence.md`).
- A profile that has no `[models.<profile id>]` entry is NOT served by that route. There is no
  pass-through of unknown model names: an aggregator serving 200 models gets entries for the
  ones we have profiles for.
- Changing `endpoint`, `adapter` or the account behind `credential` under an unchanged
  `origin_route` changes what `Origin.route` means and breaks ADR-0033 silently. Rule: do not; add a new id. (Not
  machine-checked in this step.)

### 1.2a Account file (ADR-0139)

```toml
# accounts/<id>.toml — who is billed and how p1 authenticates
id      = "work"                         # must equal the file stem
origins = ["https://api.example.test"]   # endpoint origins this credential may be sent to
[credential]                             # the route file's `[credential]` table
method  = "api-key"                      # `kind` is the same field's legacy spelling
env     = "WORK_API_KEY"
store_only = true
```

- A route names its credential inline (`[credential]`, its IMPLICIT account: id, store key
  and label are the route id, origins its endpoint origin), or a default `account = "<id>"`,
  or neither. Both together are a load error.
- The account a run uses: the environment's `account`, else the route's default account,
  else its inline credential, else the only account (file or implicit) whose `origins`
  cover the route's endpoint origin; otherwise naming the route is an error that lists the
  candidates. The environment's `account` makes its provider key `<route>@<account>`; the
  catalog registers every route × covering account pair under that key.
- A pair whose endpoint origin the account does not list is refused at assembly and before
  every credential access. Declaring an origin approves nothing: ADR-0110's compiled and
  stored approvals still decide, keyed by the account's store identity, and a store
  identity or route id that p1 ships keeps its compiled origin.
- `Origin.route` of a pair is the route's `origin_route` for its implicit account and
  `<origin_route>@<account id>` for any other, so a session that moves to another account
  drops that account's opaque reasoning (ADR-0049, owner decision D39).

### 1.3 Environment file

```toml
route   = "opencode-go-subscription"
profile = "deepseek-v4.1-flash"
```

replaces `provider`, `model` and `family`. `family` is taken from the profile. An optional
`account = "<id>"` selects the account the route is used with (ADR-0139).

The old form stays valid for ONE purpose: a catalog key that is registered as a whole provider
and consumes no profile — the first-party adapters until step 4 moves them, and test fakes
permanently. Rules, all load errors otherwise:

| keys present | meaning |
|---|---|
| `route` + `profile` | new form. `provider`, `model`, `family` must be absent. |
| `provider` + `model` + `family` | old form. `route`, `profile` must be absent. |
| anything else | error naming both valid forms |

A `provider` key that is actually a route id (or a `route` that is actually a whole-provider
key) is an error that says which form to use — no fallback from one to the other.
The shipped `deepseek` and `glm` environments move to the new form in this step; `claude` and
`gpt` move in step 4. (`claude-delegating` is gone: every main agent has the worker tools now,
ADR-0050.)

## 2. Resolution

`p1-assembly` stays free of file formats for routes and of any provider crate. It gains:

```rust
pub struct ProviderSpec {
    pub key: String,                        // whole-provider key, or route id
    pub model: String,                      // old form: configured model; new form: wire model
    pub profile: Option<Arc<ModelProfile>>, // Some exactly in the new form
    pub limits: EffectiveLimits,            // min(profile, binding); None = unknown
}
```

The HOST resolves, before calling `assemble`:

1. load the environment; if new form → load `profiles/<profile>.toml`, `routes/<route>.toml`;
2. `binding = route.models[profile.id]` or error
   `route "<r>" does not serve profile "<p>" (it serves: …)`;
3. `limits = min` of profile ceiling and binding limit per field (unknown + known = known);
4. register/lookup the adapter constructor for `route.adapter`, handing it the typed route
   data, the binding, the profile and a credential source built from `route.credential`.

The catalog keeps ONE map of provider factories. `register_providers` registers, for every
route file found, a factory under the route id whose closure captures that route's data and
selects a release provider component for its adapter key. `load_environment` retains the exact
profile text it parsed beside the selected environment and passes that text through `ProviderSpec`
to the component, even if the profile file changes before activation; lock selection still uses
the full search path. A route id that
collides with a whole-provider key is a start-up error.

## 3. Validation by lowering

"Same lowering logic" means: the adapter exposes the pure function it already uses to build a
request's model-dependent part, and both `Provider::validate` and the constructor call it.
Nothing is validated by a second, parallel table of booleans.

Constructor time (profile × dialect × binding), error = assembly fails:
- the profile's thinking policy has no encoding in this adapter/dialect;
- the behaviour needs an extension the binding does not declare (e.g. retained thinking).

`validate(request)` time, as today but against the RESOLVED instance:
- effort not in `profile.efforts` → error listing them; absent effort → `default_effort`;
- explicit `max_output_tokens` above `limits.output` → error (never silently clamped);
- tool declaration kinds the resolved instance cannot carry;
- native options: an option in ANOTHER adapter's namespace is now an ERROR
  (`option "anthropic.x" is not consumed by route "<r>" (adapter openai-chat)`), because
  silently dropping an explicit preference when switching routes is exactly the portability
  trap this split exists to remove. Options without a namespace keep their meaning.

`RouteDescription` describes the resolved instance and gains:

```rust
pub cache_key: CacheKeySupport,   // Unsupported | Optional
```

The host generates a cache key iff `Optional` and none was configured, and assembles ONCE.
An explicitly configured key on an `Unsupported` instance stays an error.
`assemble_with_cache_key`'s retry-after-any-error is deleted. Because the description is only
available from a built provider, assembly builds the provider first, reads the description,
then finalises options — the tools are still built once.

## 4. Origin, replay, journal

Unchanged: `Origin { route: <route.origin_route>, model: <binding.wire_model> }`. The existing origin
strings of the two chat routes are kept byte-for-byte, so sessions recorded by the pre-split
adapter stay resumable. The session header additionally records `profile` (id + revision) for
reproducibility; it takes no part in the resume decision (ADR-0033 compares origin only).

## 5. Tests that define done

- `p1-model-profile`: parse/validate table (unknown key, default not in efforts, stem ≠ id).
- host route loading: secret-looking header rejected; unknown adapter key lists known ones;
  unknown `credential.kind`; settings with an unknown key rejected by the adapter's struct.
- environment forms: every row of the table in 1.3, incl. the two "wrong form" errors.
- **two routes, one profile** (the point of the split): two synthetic chat routes with
  different endpoint, headers, wire model name and session header, same dialect, same profile,
  scripted transport. Assert: the model-related body fields are IDENTICAL after removing
  `model`; the declared route differences appear and nothing else differs; the origins differ;
  a journal from one is refused by the other (`RouteChanged`).
- a profile whose behaviour the dialect cannot encode → assembly error naming both.
- effort/limit/native-option errors of section 3, each through `assemble`, not only the adapter.
- cache key: `Optional` + none configured → generated, one assembly (count factory calls);
  `Unsupported` + none → absent; `Unsupported` + explicit → error.
- the conformance suite runs against the two shipped COMPOSED chat routes, built from the
  shipped files through the same host resolution (no hand-made constructor arguments).
- the characterization tests of step 1 stay green and untouched.

## 6. Not in this step

Moving Claude/GPT policy into profiles (step 4); `p1-auth` and the p1 credential store
(ADR-0040); user-level route/profile directories; `tool_forms` and cache policy in profiles;
a replay codec envelope (added when a second layout exists within one route).

## 7. Step 4 — the first-party adapters give up their model policy

After this step every shipped environment uses `route` + `profile`; the whole-provider form
remains for test fakes only, and no adapter decides anything from a model NAME.

### 7.1 Profile: thinking policy becomes the behaviour selector

`thinking` (kebab-case in files) gains two variants; the enum is what the ADR calls the
compiled behaviour strategies:

| variant | meaning | who encodes it today |
|---|---|---|
| `effort-level` | the model takes an effort level; the server decides how much to think | Messages: `thinking {adaptive, summarized}` + `output_config.effort`; Responses: `reasoning {effort, summary: auto}` |
| `budget` | the model takes a token budget per effort; the profile carries the table | Messages: `thinking {enabled, budget_tokens}` |
| `enabled`, `preserved` | (step 2) | Chat dialects |

- `budget` requires `[thinking_budgets]` with one entry per listed effort (tokens, ≥ 1024); any
  other variant forbids the table. The numbers move out of the Anthropic adapter unchanged
  (low 4096, medium 10240, high 20480, extra-high and max 32768).
- `default_effort` becomes OPTIONAL. Absent + no effort requested = the request carries no
  thinking/reasoning fields at all — that is today's first-party behaviour and it must stay
  byte-identical. The two chat profiles keep theirs.
- An adapter that has no encoding for a variant refuses at construction (as the chat adapter
  does): Responses × `budget`, Messages × `enabled`/`preserved`, Chat × `effort-level`/`budget`.
- Effort SPELLING on the wire (`xhigh`) and wire constants that are protocol requirements
  (Messages needs `max_tokens`: default 32_000, margin 8_192 over a budget) stay in the adapter.
  `text.verbosity` stays in the Responses adapter until a second consumer exists.
- Model-name prefix matching (`is_adaptive`) is deleted. Profiles are explicit records:
  `claude-fable-5`, `claude-opus-5`, `claude-opus-5-5`, `claude-sonnet-5` (`effort-level`, all
  five efforts);
  `claude-opus-4-6`, `claude-sonnet-4-6` (`budget`); `gpt-5.6-sol` and every other GPT model a
  shipped file or test names (`effort-level`, efforts low/medium/high — which is what makes
  `extra_high`/`max` an error there, replacing the adapter's hard-coded rejection with the same
  error kind). A dated snapshot is a BINDING (`wire_model`), not a profile.

### 7.2 Route files for the two subscriptions

```toml
id           = "anthropic-subscription"
origin_route = "<today's Origin.route string, byte for byte>"
adapter      = "anthropic-messages"
endpoint     = "https://api.anthropic.com"
[credential]
kind = "claude-code-oauth"
[adapter_settings]
dialect = "claude-code-subscription"   # a finite enum of IMPLEMENTED account behaviours
```

`dialect` (spelled `account` before ADR-0139 §8; the old spelling still loads, with a warning
from `p1 env show`, and both together are an error) selects what the adapter already implements
for this kind of account — the Claude Code identity prefix, the OAuth beta/version header set —
exactly as `dialect` does for Chat: named by behaviour, compiled, never free-form headers.
Responses likewise: `dialect = "codex-subscription"`
(`store:false`, no output-cap field, account-id header, session/conversation headers).
The two OAuth credential kinds become constructible from a route file (3b rejected them with
"not yet data-driven"); their sources stay the compiled ones, unchanged.

**The second Claude subscription (ADR-0074, issue #199).** `routes/anthropic-subscription-2.toml`
is `anthropic-subscription` on the owner's second account: the same adapter, endpoint,
`dialect`, `long_context` and `[models]` table, its own `id` and `origin_route`
(`anthropic-messages/claude-subscription-2` — a different account is a new id, §1.2), and the
credential `kind = "claude-code-oauth"`, `login_dir = "~/.claude-2"` with no `store_only`: p1's
store entry for the route wins when present (`p1 login anthropic-subscription-2
--from-claude-code`), else the second account's Claude Code login is borrowed in place. The
environment `claude2` is `claude` with `route = "anthropic-subscription-2"`, so `claude2/<profile>
[:effort]` works wherever `claude/…` does — `--model`, `settings.toml`, workflow roles and their
`fallback` chains, the TUI's `/model`. A role `claude/…` with `fallback = ["claude2/…"]` moves a
step to the second account when the first one's quota is exhausted (ADR-0054: an exhausted
account is a route failure). A bare profile name is now bound in both environments, so from any
third environment it needs the `environment/profile` form.

### 7.3 Constructors and lowering

`AnthropicProvider::new(route: MessagesRoute, wire_model, profile, transport, credentials)` and
the same shape for Responses; `build_request(wire_model, &profile, &request)`. `validate` and
`build_request` share one pure lowering function per adapter (profile × options → thinking
fields / error). Native-option namespaces and `describe()` facts are unchanged.

### 7.4 What proves it

- Every characterization test (step 1) passes with its EXPECTED VALUES AND FIXTURES BYTE-IDENTICAL.
  Only call sites may change (a test-local helper may map the matrix's model names to profiles by
  the OLD prefix rule — that mapping documents what the explicit records replaced).
  The near-miss and unanchored-prefix cases keep their expected bodies through that helper.
- Conformance suites of both adapters run against providers composed from the SHIPPED route and
  profile files through the host's loading path.
- Negative assembly tests for each refused adapter × variant pair, through `assemble`.
- `claude` and `gpt` environments use the new form; their journals' origin strings are unchanged
  (a session recorded before this step resumes after it — test with a recorded header). The
  `claude-delegating` environment is gone — every main agent has the worker tools (ADR-0050).
- `WHOLE_PROVIDERS` in the host is empty or gone; the old environment form is exercised only by
  fakes registered through the test hook.
