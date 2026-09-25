# Context windows of the shipped environments — researched, with sources

Issue #125 / owner order 2026-09-25: "fix all context window sizes and auto compaction behavior for
all models we are currently using. Take special care with gpt models because they behave
differently on subscription than via API and have weird context window sizes."

This file records, per shipped environment, the context window its route serves for its wire
model, where the number comes from, and how the auto-compaction threshold was derived. The
`[context]` table of each `environments/*/environment.toml` carries the operational value and a
short version of the same reasoning; this file is the long version.

**Method.** Public sources only: vendor documentation, the OpenAI Codex CLI source on GitHub, the
OpenCode gateways' own model metadata (published as the public `models.dev` dataset and echoed by
`https://opencode.ai/zen/v1/models`), and OpenRouter's public `/api/v1/models`. No logins, no keys,
no authenticated traffic. Everything below was fetched on 2026-09-24 (UTC). A number no public
source states is recorded as **unknown** and the environment keeps the value it had; no window is
invented. A number that is neither sourced nor unknown but chosen by the owner is recorded as
**policy** and never dressed up as a measurement.

**Terms.** `window` is the capacity the route accepts (p1 `[context] window_tokens`).
`threshold` is p1's useful point (`summarize_at_tokens`), where the summarizer runs before the
window is full. The "wall" is `window - output_headroom_tokens`, the largest input p1 will send;
the reserve (`output_headroom_tokens`) is the next turn's response, and reasoning counts as output
on every model we run.

## The table

Every row states the window (sourced, or "unknown" with the carried value), the reserve and the
threshold, and whether each number is **sourced** or **policy** (lead decision 2026-09-25). A
reserve is never derived from a run total: `docs/dogfood/runs.jsonl` sums usage over every request
of a run (`scripts/run-report.py`), so it cannot size one response.

| env | route | wire model | window | reserve | summarize_at | source or policy |
|---|---|---|---|---|---|---|
| claude | anthropic-subscription | claude-sonnet-5 (route also binds opus-5-5, opus-5, fable-5, sonnet-4-6, opus-4-6) | 1,000,000 | 32,000 | 500,000 | window **sourced**: owner-confirmed 2026-09-25 (ADR-0063: `long_context = true` sends the `context-1m-2025-08-07` beta) + docs.anthropic.com/en/docs/build-with-claude/context-windows ("1M tokens for Claude Sonnet 5 and Claude Sonnet 4.6"). reserve 32,000 + threshold 500,000: **owner hotfix**, not to be lowered |
| gpt | openai-codex-subscription | gpt-5.6-sol (route also binds gpt-5.6-luna, gpt-5.6-terra, gpt-5.5, gpt-6-sol, gpt-6-luna, gpt-6-astra) | 272,000 | 32,000 | 220,000 | window **sourced**: github.com/openai/codex, `codex-rs/models-manager/models.json` at main `cf792c3a` ("Model metadata returned by the Codex backend `/models` endpoint"): `"context_window": 272000`, `"max_context_window": 872000`, `"auto_compact_token_limit": null`, `supports_experimental_context: false`; `codex-rs/protocol/src/openai_models.rs` derives auto-compaction at 90 % (244,800) and usable input at 95 % (258,400). reserve 32,000 **policy** (the completion ceiling is 128,000; Codex's own 5 % = 13,600 is smaller than one reasoning-heavy xhigh turn, and no per-response measurement exists); threshold 220,000 keeps the wall at 240,000 and leaves 52,000 for one response — above that the window is hit: **accepted risk** |
| deepseek | opencode-go-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | window **sourced**: models.dev/api.json provider `opencode-go` (`limit: {context: 1000000, output: 384000}`) + api-docs.deepseek.com ("CONTEXT LENGTH 1M"). reserve 96,000 **policy** (below the 384,000 per-response ceiling; no per-response measurement exists — `split4a2-deepseek`'s 71,337 reasoning / 141,126 output are sums over 235 requests, per `docs/dogfood/runs.jsonl` and `scripts/run-report.py`); wall 904,000, threshold 300,000 → 604,000 of room, so the reserve cannot bind first |
| deepseek1 | opencode-go-1-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `deepseek` (same endpoint, another account) |
| deepseek2 | opencode-go-2-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `deepseek` |
| deepseek3 | opencode-go-3-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `deepseek` |
| cline | cline-pass-1 | cline-pass/deepseek-v4.1-flash (env default; route also binds cline-pass/glm-5.3-flash) | 1,000,000 | 96,000 | 300,000 | window **not documented on this route**: docs.cline.bot/getting-started/clinepass publishes no context window (its price table footnotes "DeepSeek API pricing"), so 1,000,000 is the MODEL's window (DeepSeek vendor + OpenCode Go gateway metadata above), carried — the runs behind these values ran on OpenCode Go, not api.cline.bot. reserve **policy** as `deepseek` |
| cline2 | cline-pass-2 | cline-pass/deepseek-v4.1-flash (env default; route also binds cline-pass/glm-5.3-flash) | 1,000,000 | 96,000 | 300,000 | as `cline` (same endpoint, another account) |
| cline (profile `glm-5.3-flash-clinepass`) | cline-pass-* | cline-pass/glm-5.3-flash | 1,000,000 | 96,000 | 300,000 | as the `cline` environment: this profile states no context or output capacity, so the effective settings come unchanged from the environment |
| zen | opencode-zen-1 | space-bunny-free (env default; route also binds mimo-v2.6-flash-free, muse-spark-1.3-contributor-free) | 1,048,576 | 524,288 | 500,000 | window **sourced**: models.dev/api.json provider `opencode`, `space-bunny-free`: `limit: {context: 1048576, input: 524288, output: 524288}` (the Zen/Go docs state no limit and `/zen/v1/models` returns only ids). reserve 524,288 = the model's documented output limit; threshold 500,000 = min(500k, 60 %) |
| zen2 | opencode-zen-2 | space-bunny-free | 1,048,576 | 524,288 | 500,000 | as `zen` |
| zen3 | opencode-zen-3 | space-bunny-free | 1,048,576 | 524,288 | 500,000 | as `zen` |
| zen (profile `mimo-v2.6-flash-free`) | opencode-zen-* | mimo-v2.6-flash-free | 200,000 | 32,000 | 120,000 | **sourced, enforced by the host**: models.dev/api.json provider `opencode`, `mimo-v2.6-flash-free`: `limit: {context: 200000, output: 32000}` (the paid/openrouter `xiaomi/mimo-v2.6-flash`, 1,048,576, is a different binding). The host folds the selected profile in (`config_for_route`), so selecting this profile narrows the 1M table to these numbers |
| zen (profile `muse-spark-1.3-contributor-free`) | opencode-zen-* | muse-spark-1.3-contributor-free | 1,048,576 | 131,072 | 500,000 | **sourced, enforced by the host**: models.dev/api.json provider `opencode`: `limit: {context: 1048576, output: 131072}`. The window is not narrowed; the profile's output ceiling lowers the reserve |
| glm | glm-subscription | glm-5.3 | 260,000 | 32,000 | 150,000 | window **unknown on the coding plan** (kept conservative): docs.z.ai/guides/llm/glm-5.3.md documents the model with 1M context and 128K output, but the plan pages conflict — docs.z.ai/devpack/tool/others.md ("glm-5.2 is 1000000; other models 200000") vs docs.z.ai/devpack/latest-model.md ("Set Context Window Size to 1000000", 1M needs the `[1m]` suffix), and our route sends the unsuffixed model. Measured: one request reached ~203k real input tokens (run `split3c-glm`). reserve 32,000 **policy** (below the documented 128,000 output); wall 228,000, threshold 150,000 → 78,000 for one response: **accepted risk** |
| kimi | kimi-coding-subscription | k3 | 262,144 | 32,000 | 150,000 | window **sourced, tier-dependent**: www.kimi.com/code/docs/en/kimi-code/models.html — "Context window 1048576 (for higher-tier members)", "on a Moderato / Plus plan, k3 supports up to 256K context; up to 1M context is available on Allegretto / Pro or above", `k3-256k` fixed at 262144; the owner's tier is unknown, so the floor is used. reserve 32,000 **policy** (below the documented 131,072 output); wall 230,144, threshold 150,000 → 80,144 for one response: **accepted risk** |

## GPT on the subscription (the special case)

The ChatGPT-login route is not the API: the same profile on `api.openai.com` is 1,050,000
(OpenRouter's public list for `openai/gpt-5.6-sol`: `context_length 1050000`,
`max_completion_tokens 128000`), while the Codex CLI's own model metadata for ChatGPT auth says
272,000 with a 95 % usable share and auto-compaction at 90 %.

Codex counts the window as follows (`codex-rs/protocol/src/openai_models.rs`):

- `resolved_context_window()` = `context_window` (272,000), falling back to `max_context_window`.
- `usable_context_window()` = `resolved × effective_context_window_percent / 100` = 95 %, i.e.
  258,400 "after reserving headroom for system prompts, tool overhead, and model output".
- `auto_compact_token_limit()` = the model's own field when present, else 90 % of the window
  (244,800); a present field is clamped to 90 %.

`environments/gpt/environment.toml` follows that accounting, with two deliberate deviations, both
**lead policy** (2026-09-25) rather than a reading of the source:

- `window_tokens = 272000` — the subscription's context window (sourced, above);
- `output_headroom_tokens = 32000` — the next turn's response. Codex's own 5 % is 13,600, but
  reasoning counts as output and the public API face of the same model allows up to 128,000
  completion tokens, so 13,600 is smaller than one reasoning-heavy xhigh turn can be; no
  per-response measurement of this route exists, so 32,000 is policy. Wall: 240,000;
- `summarize_at_tokens = 220000` — below that wall (240,000) and below Codex's derived
  `auto_compact_token_limit` of 244,800; it leaves 52,000 for one response, and a single xhigh
  response above 52,000 would hit the window: **accepted risk**, to be revisited with per-response
  telemetry (deferred follow-up below);
- `max_context_window` (872,000) is not used: it is the ceiling for an experimental context the
  subscription models do not announce (`supports_experimental_context: false` for every model our
  route binds), and overstating a window fails the request.

## Thresholds and reserves

Except where a route pins a number, `summarize_at_tokens = min(500_000, 60 % of window)` rounded to
10k, and `summarize_at < window - output_headroom` as `ContextSettings::validate` requires:

| window | 60 % | threshold | reserve |
|---|---|---|---|
| 1,000,000 (claude, deepseek*, cline*) | 600,000 → capped at 500,000 | claude 500,000 (owner hotfix); deepseek*/cline* 300,000 | claude 32,000 (owner hotfix); deepseek*/cline* 96,000 (policy) |
| 1,048,576 (zen*, space bunny / muse spark) | 629,146 → capped at 500,000 | 500,000 | the model's output limit (524,288 space bunny, 131,072 muse spark) |
| 272,000 (gpt, Codex subscription) | — (Codex's own rule) | 220,000, below both the 240,000 wall and Codex's 244,800 | 32,000 (policy) |
| 262,144 (kimi) | 157,286 → rounded down | 150,000 | 32,000 (policy) |
| 260,000 (glm, unconfirmed) | 156,000 → rounded down | 150,000 (unchanged) | 32,000 (policy) |

A reserve must cover the next turn's OUTPUT, and reasoning is output. **Every reserve here is
either the model's documented output limit (zen) or lead policy (all the others, 2026-09-25):**
no per-response output measurement exists for any of these routes, and the totals in
`docs/dogfood/runs.jsonl` (e.g. `split4a2-deepseek`'s 141,126 output / 71,337 reasoning over 235
requests) are run aggregates that cannot size one response. A policy reserve is a number below the
route's documented per-response output ceiling; where the room between the threshold and the wall
is smaller than that ceiling, the risk is recorded in the row above. Where a profile's own output
ceiling is smaller than the environment's reserve, the host lowers the reserve to the profile's
(`config_for_route`).

**Why deepseek*/cline* keep 300,000.** No public source argues for a different number: the gateway
metadata and the vendor both say the window is 1,000,000 (60 % would be 600,000, capped at
500,000), and the only measured evidence (run `split4a`, `docs/dogfood/runs.jsonl`) is that
thresholds *below* ~100k make a multi-file task thrash — it does not measure the right point.
Task rule: keep 300,000 unless a documented reason to change it exists; none was found, so the
value stands. The reserve rose to 96,000 as lead policy; with the wall at 904,000 and the threshold
at 300,000 there are 604,000 tokens of room, so the reserve cannot bind before summarization.

**claude** keeps the owner's hotfix (1,000,000 / 500,000 / reserve 32,000): ADR-0063, do not
lower.

**Manual trigger (ADR-0076).** The threshold is not the only trigger: `/compact` in the TUI and
`--compact` on `--resume` run the same summarizer on the current history now, through the same
function and the same `ContextReplaced` record, whatever `summarize_at_tokens` says. Nothing in
the table above changes; the summary uses the same reserve, summary output cap and effort. A
history with no unit older than the `keep_recent_tokens` tail is left alone
(`nothing to compact: <tokens> tokens`); otherwise the line is
`compacted: <before> → <after> tokens` (estimates). In the TUI a `/compact` typed during a turn
waits for the turn's end, like `/model`.

## Profile `context_tokens` per env

`profiles/*.toml` states what a model serves, and the environment states the window of its
ROUTE. p1 lets a narrower profile be selected on the same environment, so the host folds the
selected profile's own capacity into the effective table (`config_for_route` in
`crates/p1-host/src/run.rs`):

- effective `window_tokens` = `min(env window, profile.context_tokens)`;
- effective `output_headroom_tokens` = `min(env reserve, profile.max_output_tokens)`, and always
  strictly below the effective window (a reserve as large as the window would leave no room at all
  for the request that carries the next response);
- effective `summarize_at_tokens` = `min(env threshold, 60 % of the effective window)` only when
  the selected profile narrows the environment window; otherwise the environment's own threshold
  is retained (GPT keeps 220,000 of 272,000). The resulting threshold is always below
  `window - reserve`;
- the copied verbatim budgets (`keep_recent_tokens`, `user_verbatim_tokens`) are clamped below the
  wall, because a kept tail larger than what a request can carry would keep the whole history
  verbatim and leave the next request over the wall (a 40,000-token profile on `zen`);
- the summary-output cap (`[context] summary_output_tokens`) is clamped to half the effective wall,
  since the summarization request also carries the rendered transcript; a table that cannot host
  1,000 tokens of summary output fails the agent's construction with an error naming the profile
  and the window it serves.

Examples on `zen*` (whose table describes its DEFAULT profile, `space-bunny-free`):

- `space-bunny-free` (1,048,576 / 524,288) → unchanged: 1,048,576 / 524,288 / 500,000;
- `mimo-v2.6-flash-free` (200,000 / 32,000) → 200,000 / 32,000 / 120,000: before this rule the
  environment kept Space Bunny's 1M window, so a MiMo request could fail before compaction;
- `muse-spark-1.3-contributor-free` (1,048,576 / 131,072) → 1,048,576 / 131,072 / 500,000;
- a profile stating MORE than the environment (4,000,000 / 900,000) → the environment's table,
  unchanged: the fold only ever narrows;
- No other shipped profile states `context_tokens`, so no other environment is narrowed.

`crates/p1-host/src/run.rs` unit tests pin these (MiMo on `zen`, Muse on `zen`, a roomier synthetic
profile, a 40,000-token profile whose copied budgets AND summary cap are clamped — its agent still
starts and the request carries a 4,000-token cap — a profile with room for no summary at all, whose
construction fails with an error naming the profile, and an environment whose profile states
nothing). The same effective numbers are what the front end's `ctx` display is told, so the
denominator is the window the summarizer actually acts on.

## How to re-check

```sh
# Codex CLI (ChatGPT auth) metadata
curl -sS https://raw.githubusercontent.com/openai/codex/main/codex-rs/models-manager/models.json \
  | python3 -c 'import json,sys; [print(m["slug"], m.get("context_window"), m.get("auto_compact_token_limit")) for m in json.load(sys.stdin)["models"]]'
# gateway metadata for the Zen (free) and Go subscriptions
curl -sS https://models.dev/api.json | python3 -c 'import json,sys; d=json.load(sys.stdin); [print(p, k, v["limit"]) for p in ("opencode","opencode-go") for k,v in d[p]["models"].items() if k in ("space-bunny-free","mimo-v2.6-flash-free","muse-spark-1.3-contributor-free","deepseek-v4.1-flash")]'
# public API face of the same models (NOT the subscription face)
curl -sS https://openrouter.ai/api/v1/models | python3 -c 'import json,sys; [print(m["id"], m["context_length"], m["top_provider"]["max_completion_tokens"]) for m in json.load(sys.stdin)["data"] if m["id"] in ("openai/gpt-5.6-sol","z-ai/glm-5.3","moonshotai/kimi-k3")]'
```

## Open items (not changed here)

- **glm**: the coding plan's real window for `glm-5.3` (no `[1m]` suffix) is unresolved. If the
  plan's 1M context is confirmed for this endpoint, `routes/glm-subscription.toml` should send the
  suffixed model and `environments/glm` can move to 1,000,000/500,000. Route files and the model
  name are outside this task's owned paths.
- **kimi**: raise the window to 1,048,576 once the owner's plan tier (Allegretto/Pro vs
  Moderato/Plus) is known.
- **claude, glm, kimi reserves**: all three keep 32,000 (policy / owner hotfix), while the public
  face of their models states a 128,000/131,072 output ceiling, and on glm and kimi the room
  between the threshold and the wall (78,000 / 80,144) is smaller than that ceiling. The reserves
  stand as lead policy with the risk recorded in the table; revisit with per-response telemetry.
- **cline\***: no ClinePass window is published; the carried 1,000,000 is a model-level value.
- **gpt**: the 872,000 `max_context_window` becomes usable only if the subscription enables
  experimental context (`supports_experimental_context`).
- **First-attempt truncation telemetry** (deferred at the lead's decision): the summarizer knows
  whether attempt 1 stopped at `MaxOutputTokens`, but `Prepared` and `RecordBody::ContextReplaced`
  carry only summed `Usage`, so `scripts/run-report.py` and `scripts/usage-audit.py` cannot tell a
  one-shot replacement from a truncation retry. Reporting the first-attempt truncation rate needs
  a defaulted field through `p1-contracts`' journal, which this PR does not touch. Follow-up.
- **deferred: call-site tests for agent_context (review 146 r2)** — the host tests drive
  `agent_context` directly, not the parent / `/model` switch / worker-start / re-grant call sites;
  the lead files that issue.
