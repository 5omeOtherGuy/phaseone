# Context control — specification

Long sessions overflow the model's context; before that, they get worse. Context control is a
POLICY MODULE behind the existing seam (`ContextPolicy`, core §3b): the core never learns how
a history is shortened, only that it was replaced — durably. Without the module the harness
behaves as today (passthrough). Requirements come from the owner's failure F5/F6 and the
review of 2026-09-20 (plan amendment 4).

Two numbers must not be confused. A model's **capacity** (`window_tokens`) is what the route
accepts. The **useful point** (`summarize_at_tokens`) is where this model's work starts to
degrade or cost too much — an empirical, per-environment setting, usually far below capacity.
The policy acts at the useful point and treats capacity (minus output headroom) as a wall.

## 1. Contract changes (`p1-contracts`, `p1-core`, `p1-testkit`)

```rust
pub struct ContextInput<'a> {
    pub history: &'a [Item],
    /// Usage of this agent's most recent COMPLETED response, if it reported any. After a
    /// resume: the last journalled `AssistantCompleted.usage`.
    pub last_usage: Option<&'a Usage>,
    /// The turn's cancellation token. `prepare` must stop waiting when it fires.
    pub cancel: &'a CancellationToken,
}
pub struct Prepared {
    pub items: Vec<Item>,
    /// What preparing cost (e.g. a summarization request). `None` = unknown, never zero.
    pub usage: Option<Usage>,
}
pub enum ContextError { Cancelled, Failed(String) }   // Display: "context preparation was cancelled" / "context preparation failed: {0}"
pub trait ContextPolicy: Send + Sync {
    fn prepare<'a>(&'a self, input: ContextInput<'a>) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>>;
}
```
- `RecordBody::ContextReplaced { items, usage: Option<Usage> }` — `usage` is `#[serde(default)]`
  so journals written before this change still load.
- New `AgentEvent::ContextReplaced { items_before: usize, items_after: usize, usage: Option<Usage> }`,
  emitted only AFTER the record is committed (core ruling R6).
- Core §3b becomes: build `ContextInput`; race `prepare` against `cancel` (`biased`, cancel
  first). `Ok(None)` → unchanged. `Ok(Some(p))` → `{ContextReplaced{items: p.items, usage: p.usage}}`,
  history IS `p.items`, `[ContextReplaced{..}]`. `Err(Failed(m))` → `TurnEnd::ContextFailed{message: m}`
  as today. `Err(Cancelled)` or `cancel` firing first →
  `{AssistantInterrupted{Cancelled, partial_text: "", error: None}}`, turn ends `Cancelled`
  (the same records as a cancel before the request, ruling R1). Nothing of an abandoned
  preparation is committed.
- The core keeps `last_usage` (set at every `AssistantCompleted`, to `None` when that response
  reported none); `project()` returns it as `Projection.last_usage` and `Agent::resume` restores it.
- Core validation of a replacement, BEFORE committing it: every `ToolResult` item's `call_id`
  belongs to a `ToolCall` of an EARLIER `Assistant` item in `items`, and every `ToolCall` of an
  `Assistant` item that is not the last item has its `ToolResult` in `items`. A violation is
  `TurnEnd::ContextFailed{message: "context policy returned an unpaired tool call or result: <call_id>"}`
  and nothing is committed. A policy bug must not become a provider 400 three requests later.
- `p1-testkit`: `PassthroughContext`, `ReplacingContext` follow the new signature;
  `ReplacingContext` gains `.with_usage(Usage)`; new `GatedContext` (waits on a `Notify`, honours
  `input.cancel`) for the cancellation tests.

## 2. The module — crate `p1-context`

Depends on `p1-contracts` only. It summarizes through the ORDINARY provider interface — the
same `Arc<dyn Provider>` the agent uses, so the route, credentials and model family are the
agent's own.

```rust
pub struct ContextConfig {
    pub window_tokens: u64,            // capacity of this model on this route
    pub output_headroom_tokens: u64,   // reserved for the next response
    pub summarize_at_tokens: u64,      // the useful point; must be < window - headroom
    pub keep_recent_tokens: u64,       // newest part of the history kept verbatim
    pub user_verbatim_tokens: u64,     // budget for user messages kept verbatim
    pub tool_result_excerpt_chars: usize, // per tool result, when rendered for the summarizer (default 2_000)
}
impl ContextConfig { pub fn validate(&self) -> Result<(), String>; }
pub struct SummarizingContext;  // impl ContextPolicy
impl SummarizingContext {
    pub fn new(provider: Arc<dyn Provider>, options: ModelOptions, config: ContextConfig, prompt: String) -> Result<Self, String>;
}
pub const DEFAULT_SUMMARIZER_PROMPT: &str;
pub const SUMMARY_MARKER: &str;            // first line of every summary item, see Replacement
pub fn estimate_tokens(items: &[Item]) -> u64;   // ceil(chars / 3.5) over all model-visible text incl. tool inputs; replay payloads count too
```

**When.** `next_input = known + estimate_tokens(items added since that response)`, where
`known = input_uncached + cache_read + cache_write + output` of `last_usage` when
`input_uncached` and `output` are both known (missing cache parts count as 0); otherwise
`next_input = estimate_tokens(whole history)`. "Items added since" = everything after the last
`Assistant` item. `next_input < summarize_at_tokens` → `Ok(None)`.

**What is kept verbatim.**
1. *The recent tail*: the longest suffix of UNITS whose estimate fits `keep_recent_tokens`, but
   at least the last unit. A unit is an `Assistant` item together with all `ToolResult` items
   of its calls; `User`/`Inbox` items between units belong to the unit that follows them. Units
   are never split, so pairing holds by construction, and the `Assistant` items keep every
   block — reasoning with its replay data byte-exact.
2. *The user's words*: every `Item::User` outside the tail that is not a summary item stays a
   separate `Item::User`, verbatim, in order. If they exceed `user_verbatim_tokens`, the FIRST
   one (the task) and the newest ones that fit are kept and the rest are rendered into the
   summarizer input like any other item.
Everything else — older units, inbox items, a previous summary — is rendered as text and summarized.

**Replacement** = `[User(summary item)] + kept user messages + tail`. The summary item's text is
`SUMMARY_MARKER` + `\n` + the model's summary, where
`SUMMARY_MARKER = "[p1 context summary v1 — written by the harness from the earlier part of this session. The user's own messages follow verbatim.]"`.
An item starting with the marker is a summary item: never counted as a user message, and
re-summarized together with the newer material next time (rolling).

**The summarization request.** `system_prompt` = the configured prompt; `history` = ONE
`Item::User` holding the rendered transcript; `tools` = empty; `options` = the agent's options
with `max_output_tokens = Some(min(existing, 4_000))` where the route validates it (if
`provider.validate` rejects the request because of `max_output_tokens`, retry validation once
without it — the Codex route refuses the field). Rendering, one block per item, in order:
`## Previous summary` (the old summary text), `## User` / `## Notification` / `## Steering`,
`## Assistant` (text blocks; reasoning TEXT is omitted; each call as
`→ <tool>(<input, first 500 chars>)`), `## Result of <tool> [<status>]` with the content cut
to `tool_result_excerpt_chars` (head and tail halves, `[… n chars omitted …]` between). If the
rendered text alone would exceed `window_tokens - output_headroom_tokens - 4_000` by estimate,
drop the OLDEST rendered items after the previous summary and put
`[<n> earlier items omitted: the session was too long to summarize in one pass]` in their place.
The answer is the concatenated text blocks of the completed response; an empty answer is a failure.

`DEFAULT_SUMMARIZER_PROMPT` asks for these sections, in this order, and says what each is for:
`## Task` · `## Constraints and instructions` (every rule the user or the repository imposed —
copied forward from a previous summary, never dropped unless the user revoked it) ·
`## Decisions` (what was decided and why; same carry-forward rule) · `## State of the work`
(done / in progress / not started, with file paths) · `## Verified facts` (commands run and
their results that still matter) · `## Open problems` · `## Next step`. It forbids inventing
limits, time estimates or instructions that are not in the transcript (owner failure F3).

**Failure and cancellation.**
- `input.cancel` fires → the provider stream is dropped, `Err(ContextError::Cancelled)`. No partial summary is ever returned.
- **Nothing to summarize** — outside the tail and the kept user messages there is nothing, or
  only a previous summary: NO request is made. Below the wall → `Ok(None)`; at the wall →
  `Err(Failed("context is full (<next_input> of <window> tokens) and nothing is left to summarize"))`.
  Without this rule one oversized unit would buy a useless summarization before every request.
- The summarization fails (provider error, empty answer): if `next_input < window_tokens - output_headroom_tokens` → `Ok(None)`
  — the turn goes on with the full history and the next request tries again; otherwise
  `Err(Failed("context is full (<next_input> of <window> tokens) and summarizing failed: <reason>"))`.
- After a successful replacement the estimate of the new history must be below
  `summarize_at_tokens`; if it is not (tail + user messages alone are too big) the policy
  halves `keep_recent_tokens` (never below one unit) and rebuilds, up to 3 times, before failing as above.

**Durability and resume.** The module keeps NO state across calls: everything it needs is in
the history (the marker) and `last_usage`. A crash during summarization leaves no record — the
resumed agent meets the same threshold and tries again. A committed `ContextReplaced` IS the
history on resume (journal.md projection rule), with `last_usage` restored.

**Rulings (after the independent test author's ambiguity list, 2026-09-20).** Rendered blocks
are separated by ONE blank line. `<status>` in a result heading is the snake_case name the
journal uses (`ok`, `error`, `unavailable`, `denied`, `cancelled`, `unknown`). "Under the wall"
is inclusive (`<=`). `User`/`Inbox` items after the last unit belong to the tail and are always
kept. Validation and soft-failure reason texts are free, except the `Failed` message at the wall.
The first draft also called a replacement "not smaller than the original" a failure; that
contradicted the frozen suite (a forced single-unit tail makes the replacement larger by
construction) and is withdrawn — the "nothing to summarize" rule is what prevents the loop.

## 3. Assembly and host

`environment.toml` gains an optional table; absent = passthrough, as today:
```toml
[context]
window_tokens = 200000
output_headroom_tokens = 16000
summarize_at_tokens = 120000
keep_recent_tokens = 30000
user_verbatim_tokens = 8000
```
plus an optional `summarize.md` next to `prompt.md` (whole-file override of the prompt, the
model-family seam). `ResolvedEnvironment` shows the table and the effective prompt.
`AssemblyError::InvalidContext{message}` when `validate` fails. `p1-assembly` parses and
resolves; the HOST constructs `SummarizingContext` (composition root) for the parent and for
every worker from its own environment. The renderer prints
`context: summarized <before> → <after> items · <usage line>` on `ContextReplaced`.
The shipped environments get measured values only after dogfooding; until then they ship
WITHOUT `[context]`.

## 4. Must-pass behaviour (deterministic, scripted provider)

a. Below the threshold: `Ok(None)`, no provider request.
b. Above it: one summarization request whose only history item contains the rendered
   transcript and no tools; the replacement is `[summary] + user messages + tail`; the task text
   is byte-identical; every `Assistant` item of the tail is `==` the original (replay data too).
c. Pairing: for histories where the tail boundary would fall between a call and its result, or
   inside a multi-call response, the unit is kept whole (property: core validation accepts
   every replacement the module produces, over generated histories).
d. **Repeated replacements**: 5 rounds of grow-and-replace — the first user message and a user
   message stating a constraint stay byte-identical after every round; each round's request
   contains the previous summary under `## Previous summary`; the history holds exactly one
   summary item.
e. Threshold arithmetic: known usage + estimate of later items; unknown usage → whole-history
   estimate; cache parts missing → counted as 0, not as unknown.
f. Cancellation while the summarization request is in flight → `Err(Cancelled)`; through the
   core: records `…, AssistantInterrupted{Cancelled}`, no `ContextReplaced`, turn `Cancelled`.
g. Failure below the wall → `Ok(None)` and the NEXT prepare tries again; failure at the wall →
   `Failed` naming both numbers.
h. Oversized transcript → oldest rendered items dropped with the omission line; the request
   estimate is under the wall.
i. Through the core + JSONL journal: replace, continue, kill, resume → history equals the
   replaced history plus what followed; `last_usage` restored; a second replacement works
   after the resume.
j. The Codex-style route that rejects `max_output_tokens` still gets a valid request.

Evidence that cannot be scripted — that a real model's summary carries constraints through
repeated replacements — is a lead-run live check (`P1_LIVE=1`): a canary constraint stated
once, three forced replacements (tiny `summarize_at_tokens`), then a question only the
constraint answers. Its result is reported, not assumed.
