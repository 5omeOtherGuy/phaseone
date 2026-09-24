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
window is full. The "wall" is `window - output_headroom_tokens`, the largest input p1 will send.

## The table

| env | route | wire model | window | threshold | source (URL / file, fetched 2026-09-24) | documented or measured |
|---|---|---|---|---|---|---|
| claude | anthropic-subscription | claude-sonnet-5 (route also binds opus-5-5, opus-5, fable-5, sonnet-4-6, opus-4-6) | 1,000,000 | 500,000 | Owner-confirmed 2026-09-25 (ADR-0063: `long_context = true` sends the `context-1m-2025-08-07` beta). Public: docs.anthropic.com/en/docs/build-with-claude/context-windows — "1M tokens for Claude Sonnet 5 and Claude Sonnet 4.6" | vendor-documented + owner-confirmed |
| gpt | openai-codex-subscription | gpt-5.6-sol (route also binds gpt-5.6-luna, gpt-5.6-terra, gpt-5.5, gpt-6-sol, gpt-6-luna, gpt-6-astra) | 272,000 | 240,000 | github.com/openai/codex, `codex-rs/models-manager/models.json` at main `cf792c3a` ("Model metadata returned by the Codex backend `/models` endpoint"): gpt-5.6-sol `"context_window": 272000`, `"max_context_window": 872000`, `"auto_compact_token_limit": null`, `supports_experimental_context: false`; `codex-rs/protocol/src/openai_models.rs` `ModelInfo::auto_compact_token_limit()` derives 90 % of the window when the field is null → 244,800, and `usable_context_window()` = `effective_context_window_percent` (default 95) → 258,400 | vendor-documented (Codex CLI source, ChatGPT auth) |
| deepseek | opencode-go-subscription | deepseek-v4.1-flash | 1,000,000 | 300,000 | models.dev/api.json provider `opencode-go`, model `deepseek-v4.1-flash`: `limit: {context: 1000000, output: 384000}`; api-docs.deepseek.com (pricing/model page): `CONTEXT LENGTH 1M` | gateway metadata + vendor-documented |
| deepseek1 | opencode-go-1-subscription | deepseek-v4.1-flash | 1,000,000 | 300,000 | as `deepseek` (same endpoint, another account) | gateway metadata + vendor-documented |
| deepseek2 | opencode-go-2-subscription | deepseek-v4.1-flash | 1,000,000 | 300,000 | as `deepseek` | gateway metadata + vendor-documented |
| deepseek3 | opencode-go-3-subscription | deepseek-v4.1-flash | 1,000,000 | 300,000 | as `deepseek` | gateway metadata + vendor-documented |
| cline | cline-pass-1 | cline-pass/deepseek-v4.1-flash | 1,000,000 | 300,000 | **unknown on this route**: docs.cline.bot/getting-started/clinepass documents subscription limits and reference prices, no context window (it footnotes "DeepSeek API pricing" for the model). The value is carried from the same model's vendor/gateway numbers above | not documented on the route (carried) |
| cline2 | cline-pass-2 | cline-pass/deepseek-v4.1-flash | 1,000,000 | 300,000 | as `cline` (same endpoint, another account) | not documented on the route (carried) |
| zen | opencode-zen-1 | space-bunny-free (env default; route also binds mimo-v2.6-flash-free, muse-spark-1.3-contributor-free) | 1,048,576 | 500,000 | models.dev/api.json provider `opencode`, model `space-bunny-free`: `limit: {context: 1048576, input: 524288, output: 524288}` — the Zen/Go docs state no limit and `/zen/v1/models` returns only ids | gateway metadata |
| zen2 | opencode-zen-2 | space-bunny-free | 1,048,576 | 500,000 | as `zen` | gateway metadata |
| zen3 | opencode-zen-3 | space-bunny-free | 1,048,576 | 500,000 | as `zen` | gateway metadata |
| zen (profile `mimo-v2.6-flash-free`) | opencode-zen-* | mimo-v2.6-flash-free | 200,000 | 120,000 | models.dev/api.json provider `opencode`, model `mimo-v2.6-flash-free`: `limit: {context: 200000, output: 32000}`. The paid/openrouter `xiaomi/mimo-v2.6-flash` (1,048,576) is a different binding | gateway metadata |
| zen (profile `muse-spark-1.3-contributor-free`) | opencode-zen-* | muse-spark-1.3-contributor-free | 1,048,576 | 500,000 | models.dev/api.json provider `opencode`: `limit: {context: 1048576, output: 131072}` | gateway metadata |
| glm | glm-subscription | glm-5.3 | 260,000 | 150,000 | **unknown on the coding plan**: docs.z.ai/guides/llm/glm-5.3.md says the model has "a 1M-token context window and a maximum output length of 128K tokens", but the plan pages conflict — docs.z.ai/devpack/tool/others.md tells clients "Adjust Context Window Size based on your model (glm-5.2 is 1000000; other models 200000)" while docs.z.ai/devpack/latest-model.md tells the same client to "Set Context Window Size to 1000000" and says 1M needs the `[1m]` model suffix. Keep the previous conservative value | model documented; plan window unknown |
| kimi | kimi-coding-subscription | k3 | 262,144 | 150,000 | www.kimi.com/code/docs/en/kimi-code/models.html: `Context window 1048576 (for higher-tier members)`, `262144 only` for `k3-256k`, and "on a Moderato / Plus plan, k3 supports up to 256K context; up to 1M context is available on Allegretto / Pro or above". The owner's plan tier is unknown, so the floor is used | vendor-documented, tier-dependent |

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

`environments/gpt/environment.toml` follows that accounting:

- `window_tokens = 272000` — the subscription's context window;
- `output_headroom_tokens = 13600` — Codex's own 5 % reserve, so p1's wall equals Codex's usable
  input window (258,400);
- `summarize_at_tokens = 240000` — at or below Codex's derived `auto_compact_token_limit` of
  244,800, rounded down to 10k;
- `max_context_window` (872,000) is not used: it is the ceiling for an experimental context the
  subscription models do not announce (`supports_experimental_context: false` for every model our
  route binds), and overstating a window fails the request.

## Thresholds

Except where a route pins a number, `summarize_at_tokens = min(500_000, 60 % of window)` rounded to
10k, and `summarize_at < window - output_headroom` as `ContextSettings::validate` requires:

| window | 60 % | threshold |
|---|---|---|
| 1,000,000 (claude, deepseek*, cline*) | 600,000 → capped at 500,000 | claude 500,000 (owner hotfix); deepseek*/cline* keep the existing 300,000 |
| 1,048,576 (zen*, space bunny / muse spark) | 629,146 → capped at 500,000 | 500,000 |
| 272,000 (gpt, Codex subscription) | — (Codex's own rule) | 240,000 ≤ 244,800 |
| 262,144 (kimi) | 157,286 → rounded down | 150,000 |
| 260,000 (glm, unconfirmed) | 156,000 → rounded down | 150,000 (unchanged) |

**Why deepseek*/cline* keep 300,000.** No public source argues for a different number: the gateway
metadata and the vendor both say the window is 1,000,000 (60 % would be 600,000, capped at
500,000), and the only measured evidence (run `split4a`, `docs/dogfood/runs.jsonl`) is that
thresholds *below* ~100k make a multi-file task thrash — it does not measure the right point.
Task rule: keep 300,000 unless a documented reason to change it exists; none was found, so the
value stands.

**claude** keeps the owner's hotfix (1,000,000 / 500,000 / headroom 32,000): ADR-0063, do not
lower.

## Profile `context_tokens` per env

`profiles/*.toml` states a model's own capacity, and the environment states the window of the
selected route+model. They differ on the `zen*` environments by design:

- `profiles/mimo-v2.6-flash-free.toml` says 200,000, and **that is correct for the Zen free
  binding** (`opencode/mimo-v2.6-flash-free` = 200,000/32,000 in models.dev). The `zen*`
  environments' `[context]` describes their DEFAULT profile, `space-bunny-free` (1,048,576), and
  p1 does not narrow the window when a narrower profile is selected on the same environment
  (role-window plumbing is #113). So selecting MiMo on `zen` still sends the Space Bunny window —
  a known limitation recorded in the env comment, not a wrong profile value.
- `profiles/space-bunny-free.toml` (1,048,576 / 524,288) and
  `profiles/muse-spark-1.3-contributor-free.toml` (1,048,576 / 131,072) match the gateway metadata
  above.
- No other profile states `context_tokens`, so no other profile can contradict an environment.

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
- **zen\***: the window follows the environment's default profile, not the selected one (#113).
- **cline\***: no ClinePass window is documented; the carried 1,000,000 is a model-level value.
- **gpt**: the 872,000 `max_context_window` becomes usable only if the subscription enables
  experimental context (`supports_experimental_context`).
