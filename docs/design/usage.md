# Usage ledger

The usage snapshot and renderer live in `p1-usage`, not the host or TUI: probing and layout must be reusable by both surfaces without coupling a provider to a UI. `p1 usage` supplies loaded, deduplicated routes, credential *source descriptions*, and p1-auth's credential references; it never carries credentials in the serialized snapshot.

## Data

`Snapshot { taken_at, routes }` has an RFC 3339 UTC timestamp and one `RouteUsage` per route. A route has id, label, source wording, `Probe` (`Supported`, `Unsupported`, or `Failed` with a credential/HTTP/network/parse kind), optional plan, windows, credits and extra usage. Each `Window` has a session/weekly/scoped/other kind, optional scope, percentage and reset time, and a limit-reached flag. Unknown measurements are `None`, never zero.

| Route credential kind | Probe | Fields and mapping |
|---|---|---|
| `claude-code-oauth` | `GET https://api.anthropic.com/api/oauth/usage`, OAuth bearer, `anthropic-beta: oauth-2025-04-20` | `limits[]`: `session` → 5h, `weekly_all` → 7d, `weekly_scoped` → 7d + model display name; `percent`, `resets_at`, `extra_usage` credits/limit/currency |
| `codex-oauth` | `GET https://chatgpt.com/backend-api/wham/usage`, OAuth bearer and account id | `plan_type`, primary/secondary `used_percent`, `limit_window_seconds` (18000 → 5h, 604800 → 7d, otherwise hours), `reset_at`, `limit_reached`, `credits.balance`, `rate_limit_reset_credits.available_count` |
| `api-key` | none | Unsupported: no usage endpoint known for this route |

p1-auth alone resolves credentials; the probe makes one request with a 20-second timeout and no retries. Concurrent routes fail independently. HTTP errors retain only status and a restricted error code/type, not a body or headers.

## Rendering contract

`render(snapshot, grid)` produces palette-independent lines and spans. Tone roles are INK (values and filled bars), DIM (labels), FAINT (unavailable rows), RULE (unfilled bars). The caller maps tones to ANSI or TUI palette. The grid rule right-aligns each value with computed padding at grid width (32 or 48); percentages fill rounded fractions of `grid - 6` cells, with both filled and unfilled cells rendered as `█`. Unknown values print `—`, never `0` or blank. No border or box-drawing. Supported routes rank by highest utilization, ahead of failures and unsupported routes.

A future `USAGE` right-pane mode mounts the same lines at the pane's content grid and maps `Tone` to the SPEC §1 palette; no TUI change is needed to the module. The host's CLI maps the same roles to 24-bit terminal foreground colors.

Non-goals: per-session cost roll-up from journals, Kimi/Z.ai quota, and opencode-go credit (no known endpoints as of 2026-09-23).
