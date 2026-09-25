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
invented.

**Terms.** `window` is the capacity the route accepts (p1 `[context] window_tokens`).
`threshold` is p1's useful point (`summarize_at_tokens`), where the summarizer runs before the
window is full. The "wall" is `window - output_headroom_tokens`, the largest input p1 will send;
the reserve (`output_headroom_tokens`) is the next turn's response, and reasoning counts as output
on every model we run.

## The table

| env | route | wire model | window | reserve | threshold | source (URL / file, fetched 2026-09-24) | documented or measured |
|---|---|---|---|---|---|---|---|
| claude | anthropic-subscription | claude-sonnet-5 (route also binds opus-5-5, opus-5, fable-5, sonnet-4-6, opus-4-6) | 1,000,000 | 32,000 | 500,000 | Owner-confirmed 2026-09-25 (ADR-0063: `long_context = true` sends the `context-1m-2025-08-07` beta). Public: docs.anthropic.com/en/docs/build-with-claude/context-windows — "1M tokens for Claude Sonnet 5 and Claude Sonnet 4.6" (the same public API face lists a 128,000 completion ceiling; the 32,000 reserve is p1's next-response budget, kept with the owner's hotfix) | vendor-documented + owner-confirmed |
| gpt | openai-codex-subscription | gpt-5.6-sol (route also binds gpt-5.6-luna, gpt-5.6-terra, gpt-5.5, gpt-6-sol, gpt-6-luna, gpt-6-astra) | 272,000 | 32,000 | 220,000 | github.com/openai/codex, `codex-rs/models-manager/models.json` at main `cf792c3a` ("Model metadata returned by the Codex backend `/models` endpoint"): gpt-5.6-sol `"context_window": 272000`, `"max_context_window": 872000`, `"auto_compact_token_limit": null`, `supports_experimental_context: false`; `codex-rs/protocol/src/openai_models.rs` `ModelInfo::auto_compact_token_limit()` derives 90 % of the window when the field is null → 244,800, and `usable_context_window()` = `effective_context_window_percent` (default 95) → 258,400. The public API face allows 128,000 completion tokens, so the reserve is 32,000 rather than Codex's 5 % | vendor-documented (Codex CLI source, ChatGPT auth) |
| deepseek | opencode-go-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | models.dev/api.json provider `opencode-go`, model `deepseek-v4.1-flash`: `limit: {context: 1000000, output: 384000}`; api-docs.deepseek.com (pricing/model page): `CONTEXT LENGTH 1M`. Reserve from measured output: run `split4a2-deepseek` (`docs/dogfood/runs.jsonl`) reported 71,337 reasoning tokens / 141,126 output tokens | gateway metadata + vendor-documented + measured |
| deepseek1 | opencode-go-1-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `deepseek` (same endpoint, another account) | gateway metadata + vendor-documented + measured |
| deepseek2 | opencode-go-2-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `deepseek` | gateway metadata + vendor-documented + measured |
| deepseek3 | opencode-go-3-subscription | deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `deepseek` | gateway metadata + vendor-documented + measured |
| cline | cline-pass-1 | cline-pass/deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | **unknown on this route**: docs.cline.bot/getting-started/clinepass documents subscription limits and reference prices, no context window (it footnotes "DeepSeek API pricing" for the model). The window is carried from the same model's vendor/gateway numbers above; the reserve from the measured run | not documented on the route (carried) |
| cline2 | cline-pass-2 | cline-pass/deepseek-v4.1-flash | 1,000,000 | 96,000 | 300,000 | as `cline` (same endpoint, another account) | not documented on the route (carried) |
| zen | opencode-zen-1 | space-bunny-free (env default; route also binds mimo-v2.6-flash-free, muse-spark-1.3-contributor-free) | 1,048,576 | 524,288 | 500,000 | models.dev/api.json provider `opencode`, model `space-bunny-free`: `limit: {context: 1048576, input: 524288, output: 524288}` — the Zen/Go docs state no limit and `/zen/v1/models` returns only ids | gateway metadata |
| zen2 | opencode-zen-2 | space-bunny-free | 1,048,576 | 524,288 | 500,000 | as `zen` | gateway metadata |
| zen3 | opencode-zen-3 | space-bunny-free | 1,048,576 | 524,288 | 500,000 | as `zen` | gateway metadata |
| zen (profile `mimo-v2.6-flash-free`) | opencode-zen-* | mimo-v2.6-flash-free | 200,000 | 32,000 | 120,000 | models.dev/api.json provider `opencode`, model `mimo-v2.6-flash-free`: `limit: {context: 200000, output: 32000}`. The paid/openrouter `xiaomi/mimo-v2.6-flash` (1,048,576) is a different binding. **Effective on this environment**: the host folds a selected profile's capacity in, so selecting this profile narrows the 1M table to these numbers (`crates/p1-host/src/run.rs` `config_for_route`) | gateway metadata, enforced by the host |
| zen (profile `muse-spark-1.3-contributor-free`) | opencode-zen-* | muse-spark-1.3-contributor-free | 1,048,576 | 131,072 | 500,000 | models.dev/api.json provider `opencode`: `limit: {context: 1048576, output: 131072}`. The window is not narrowed; the profile's output ceiling lowers the reserve | gateway metadata, enforced by the host |
| glm | glm-subscription | glm-5.3 | 260,000 | 32,000 | 150,000 | **unknown on the coding plan**: docs.z.ai/guides/llm/glm-5.3.md says the model has "a 1M-token context window and a maximum output length of 128K tokens" (no 32,000 reserve is derived from it — p1's next-response budget stands until the plan's window is known), but the plan pages conflict — docs.z.ai/devpack/tool/others.md tells clients "Adjust Context Window Size based on your model (glm-5.2 is 1000000; other models 200000)" while docs.z.ai/devpack/latest-model.md tells the same client to "Set Context Window Size to 1000000" and says 1M needs the `[1m]` model suffix. Keep the previous conservative value | model documented; plan window unknown |
| kimi | kimi-coding-subscription | k3 | 262,144 | 32,000 | 150,000 | www.kimi.com/code/docs/en/kimi-code/models.html: `Context window 1048576 (for higher-tier members)`, `262144 only` for `k3-256k`, and "on a Moderato / Plus plan, k3 supports up to 256K context; up to 1M context is available on Allegretto / Pro or above". The owner's plan tier is unknown, so the floor is used | vendor-documented, tier-dependent |

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

`environments/gpt/environment.toml` follows that accounting, with two deliberate deviations
(#125 review: reasoning counts as output):

- `window_tokens = 272000` — the subscription's context window;
- `output_headroom_tokens = 32000` — the next turn's response, not Codex's 5 % (13,600): Sol
  reasons heavily at high/xhigh, and the public API face of the same model allows up to 128,000
  completion tokens, so a 13,600 reserve can be overrun by a single turn. Wall: 240,000;
- `summarize_at_tokens = 220000` — below that wall (240,000) and below Codex's derived
  `auto_compact_token_limit` of 244,800;
- `max_context_window` (872,000) is not used: it is the ceiling for an experimental context the
  subscription models do not announce (`supports_experimental_context: false` for every model our
  route binds), and overstating a window fails the request.

## Thresholds and reserves

Except where a route pins a number, `summarize_at_tokens = min(500_000, 60 % of window)` rounded to
10k, and `summarize_at < window - output_headroom` as `ContextSettings::validate` requires:

| window | 60 % | threshold | reserve |
|---|---|---|---|
| 1,000,000 (claude, deepseek*, cline*) | 600,000 → capped at 500,000 | claude 500,000 (owner hotfix); deepseek*/cline* 300,000 | claude 32,000; deepseek*/cline* 96,000 |
| 1,048,576 (zen*, space bunny / muse spark) | 629,146 → capped at 500,000 | 500,000 | the model's output limit (524,288 space bunny, 131,072 muse spark) |
| 272,000 (gpt, Codex subscription) | — (Codex's own rule) | 220,000, below both the 240,000 wall and Codex's 244,800 | 32,000 |
| 262,144 (kimi) | 157,286 → rounded down | 150,000 | 32,000 |
| 260,000 (glm, unconfirmed) | 156,000 → rounded down | 150,000 (unchanged) | 32,000 |

A reserve must cover the next turn's OUTPUT, and reasoning is output. Two reserves are derived
from measured output rather than from a round number: deepseek*/cline* at 96,000, because the
accepted run `split4a2-deepseek` (`docs/dogfood/runs.jsonl`: 235 requests, 141,126 output tokens,
71,337 reasoning tokens) already exceeded the previous 32,000; and gpt at 32,000 against the
128,000 completion ceiling of the same model on the public API. Where a profile's own output
ceiling is smaller than the environment's reserve, the host lowers the reserve to the profile's
(`config_for_route`).

**Why deepseek*/cline* keep 300,000.** No public source argues for a different number: the gateway
metadata and the vendor both say the window is 1,000,000 (60 % would be 600,000, capped at
500,000), and the only measured evidence (run `split4a`, `docs/dogfood/runs.jsonl`) is that
thresholds *below* ~100k make a multi-file task thrash — it does not measure the right point.
Task rule: keep 300,000 unless a documented reason to change it exists; none was found, so the
value stands. The reserve rose to 96,000; the wall (904,000) is still far above it.

**claude** keeps the owner's hotfix (1,000,000 / 500,000 / reserve 32,000): ADR-0063, do not
lower.

## Profile `context_tokens` per env

`profiles/*.toml` states what a model serves, and the environment states the window of its
ROUTE. p1 lets a narrower profile be selected on the same environment, so the host folds the
selected profile's own capacity into the effective table (`config_for_route` in
`crates/p1-host/src/run.rs`):

- effective `window_tokens` = `min(env window, profile.context_tokens)`;
- effective `output_headroom_tokens` = `min(env reserve, profile.max_output_tokens)` — a reserve
  larger than the effective window would leave no room at all for the request;
- effective `summarize_at_tokens` = `min(env threshold, 60 % of the effective window)`, and always
  below `window - reserve`.

Examples on `zen*` (whose table describes its DEFAULT profile, `space-bunny-free`):

- `space-bunny-free` (1,048,576 / 524,288) → unchanged: 1,048,576 / 524,288 / 500,000;
- `mimo-v2.6-flash-free` (200,000 / 32,000) → 200,000 / 32,000 / 120,000: before this rule the
  environment kept Space Bunny's 1M window, so a MiMo request could fail before compaction;
- `muse-spark-1.3-contributor-free` (1,048,576 / 131,072) → 1,048,576 / 131,072 / 500,000.
- No other shipped profile states `context_tokens`, so no other environment is narrowed.

`crates/p1-host/src/run.rs` unit tests pin these (MiMo on `zen`, Muse on `zen`, and an
environment whose profile states nothing). The same effective numbers are what the front end's
`ctx` display is told, so the denominator is the window the summarizer actually acts on.

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
- **claude, glm, kimi reserves**: all three keep 32,000, while the public face of their models
  states a 128,000/131,072 output ceiling. Raising them was NOT decided by the lead and is not a
  one-line change (on glm and kimi a 131,072 reserve would fall below the current 150,000
  threshold and force an earlier compaction), so the numbers stand with the ceiling recorded here.
- **cline\***: no ClinePass window is documented; the carried 1,000,000 is a model-level value.
- **gpt**: the 872,000 `max_context_window` becomes usable only if the subscription enables
  experimental context (`supports_experimental_context`).
- **First-attempt truncation telemetry** (deferred at the lead's decision): the summarizer knows
  whether attempt 1 stopped at `MaxOutputTokens`, but `Prepared` and `RecordBody::ContextReplaced`
  carry only summed `Usage`, so `scripts/run-report.py` and `scripts/usage-audit.py` cannot tell a
  one-shot replacement from a truncation retry. Reporting the first-attempt truncation rate needs
  a defaulted field through `p1-contracts`' journal, which this PR does not touch. Follow-up.
