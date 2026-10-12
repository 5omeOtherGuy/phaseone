# Model selection and scoped models

ADR-0049 (owner request 2026-09-21: "we are missing basic functionality. Model selection /
scoped models like in pi"). Donor for the behaviour, not the code: pi (`docs/usage.md`,
`settings.md`, `keybindings.md` of `@earendil-works/pi-coding-agent`): `--model`, `--thinking`,
`--models <patterns>` for cycling, `--list-models`, `/model`, `/scoped-models`, `enabledModels`.

## 1. What a model is in p1

An **environment** names a route and a model; its **route** × **profile** fixes the provider
(ADR-0039). Since the kit unit of #924 (ADR-0217) the environment no longer fixes the tools, the
prompt and `[context]` by itself: every table it leaves out comes from the kit of the SELECTED
model's family (`kits/<family>/`, `kits/README.md`), so `--model gpt/gpt-6-sol` or a `/model` to
a profile of another family loads that family's tools, prompt, context and options, and only the
tables the environment states itself stay. Since the selector unit of #924 the model is the
selector: a bare model (`gpt-6-sol`) is a complete reference, and p1 derives the route that
reaches it and the kit that suits it (rule 2 below). The explicit form names an environment too:

```
<environment>/<profile>[:<effort>]        e.g.  gpt/gpt-5.6-luna:high
<profile>[:<effort>]                      e.g.  gpt-6-sol:high
```

The candidates are exactly: every environment E, every profile P bound in E's route file
(`[models."P"]`). Every main agent has the worker tools (ADR-0050), so delegation is no longer an
environment property: a model is listed once per environment, and `claude/claude-opus-5` is one
model, not two. Nothing else is selectable; there is no free-text model id.

The two Claude subscriptions are two environments (ADR-0074): `claude` (route
`anthropic-subscription`) and `claude2` (an alias of `claude` on account `claude-2`, ADR-0139) bind the same profiles,
so `claude/claude-opus-5` and `claude2/claude-opus-5` are two models — the same profile on two
accounts. A bare `--model claude-opus-5` from either keeps that session's route and account;
from `gpt` two routes bind it (`anthropic`, `anthropic-subscription`), so it takes the route
`settings.toml` prefers or is refused naming both. Rule 2 below is that behaviour, pinned by
`a_bare_model_takes_the_current_route_then_the_preference_then_the_sole_route`
(`crates/p1-host/tests/models.rs`).

**Reference resolution** (`--model`, `/model`, `default_model`), in order:
1. `E/P` — that pair; unknown pair → error listing the pairs whose P or E matches.
2. bare `P` (ADR-0217 Decision 3) — the CURRENT environment (the `--env` given, else the
   default; in a session, the session's), on a route found in this order: the route the session
   runs on when it binds P (with its account); else the route `settings.toml [preferred_routes]`
   prefers for P, else for P's family (a family's entry applies only where its route binds P, and
   a profile's entry that does not bind P is an error); else the sole route of every layer that
   binds P; else an error that lists the routes (never a guess). On another route than its own
   the environment runs that route with the route's own account unless `@account` names one, and
   the kit of P's family supplies every table the environment leaves out. A worker's bare model
   resolves the same way from its own environment, without a preference.
3. `:effort` — must be one of the profile's `efforts`; absent keeps the environment's
   `[options] reasoning_effort` if the profile supports it, else the profile default.
4. `@account` (ADR-0139 §3) — the account the environment's route runs with, written last:
   `E/P[:effort][@account]`. The account must list the route's endpoint origin, else the
   selection fails before any request; absent keeps the environment's own account. Accepted
   by `--model`, `/model`, `default_model`, workflow roles and their fallbacks, and subagent
   `models`. `--account ID` selects it for the run's own model; together with a `--model`
   naming another account it is a usage error, and it replaces the account of
   `default_model`. `/effort`, a module reload and a `/model` without `@account` within the
   session's environment and route keep the session's account; `/model` to another environment
   or route runs its own account.

## 2. Stage 1 — choose at start (no core change)

- `--model REF` (with or without `--env`; together they must agree, else a usage error).
  `--effort LEVEL` overrides the effort of whatever was selected. Both work headless.
- `p1 models [SEARCH]` prints one row per model (ADR-0217), sorted: the profile id, the kit its
  family picks (`base` when no layer has one), the route a bare `--model P` takes from the default
  environment (or `one of …`, the routes it would name when it refuses), efforts (`low…max` as the
  profile lists them, `-` if none), that route's credential source (the same wording as
  `p1 env show`'s `credential` line — never a value; `-` without a route), and `default` /
  `scoped` / `shadows:` markers. SEARCH is a case-insensitive substring filter on the model.
- `~/.config/p1/settings.toml` (`$P1_CONFIG_DIR` first, ADR-0201, else XDG like the p1 store; absent = empty; unknown key = error
  naming the file and key):
  ```toml
  default_model  = "claude/claude-sonnet-5"      # replaces DEFAULT_ENV when set
  default_environment = "claude"                 # replaces DEFAULT_ENV (`default`, ADR-0197);
                                                 # default_model wins over it for a bare run
  enabled_models = ["claude/*", "gpt/gpt-5.6-sol*", "deepseek2/*"]   # the scope

  [preferred_routes]                             # ADR-0217: where a bare model goes when the
  gpt = "openai-codex-subscription"              # session's route does not bind it; by family
  "gpt-5.5" = "openai-chat"                      # or by model, the model's entry first
  ```
- `--models PATTERNS` (comma-separated) replaces `enabled_models` for this run. A pattern is a
  glob (`*`, `?`) matched against `E/P`; a pattern without `/` matches the profile part only.
  An empty scope means "every model". A pattern that matches nothing is an error (a typo must
  not silently shrink the scope).
- `p1 env show` gains the resolved `model  E/P:effort` line.

## 3. Stage 2 — switch within a session (core, supersedes ADR-0033)

- Core: `Agent::reconfigure(&mut self, next: Reconfiguration) -> Result<(), ReconfigureError>`
  is `async` — callable only between turns (`&mut self`). `Reconfiguration` carries `provider`,
  `tools`, `system_prompt`, `options`, `context` and `authorization:
  Option<Arc<dyn AuthorizationPolicy>>` (ADR-0084 item 3; `None` keeps the current policy). It
  runs exactly `assemble`'s checks (duplicate tool names; `provider.validate`) but validates
  against the CURRENT history, not an empty one; then it commits the candidate's `Environment`
  record and only once that commit returned installs every part, with no await in between, so
  the journal names the assembly that answers before it answers. A rejected candidate or a
  failed commit changes nothing and reports the reason as `ReconfigureError`. This amends
  ADR-0049's lazy commit for this path only (the first turn's commit and resume are unchanged):
  see `docs/design/modules/adr-drafts/asynchronous-reconfiguration-commits-before-it-installs.md`.
- Resume: `ResumeError::RouteChanged` is removed. A journal whose last origin differs from the
  assembled provider's resumes iff the provider validates the projected history — the same rule
  as a live switch, so a session can always be resumed on the model it was switched to, and on
  any model a live switch would have accepted.
- Providers: `validate` gains the history check. Each adapter carries every history item it can
  and rejects, with a sentence naming the item, what it cannot. Required translations (no new
  wire behaviour): a call of a kind the route has no native shape for (a freeform `apply_patch`
  call on the Messages or Chat route) is sent as a function-shaped call whose arguments are
  `{"input": <raw text>}`; foreign reasoning is dropped (unchanged, providers.md). Calls to
  tools the new environment does not declare stay in the history as they are.
- Switching happens only at a turn boundary, never inside a tool loop (Anthropic requires the
  current tool loop's thinking blocks; a boundary has none pending).
- Live acceptance — PASSED 2026-09-21 (table in ADR-0049): sonnet-5 → opus-5, sol → luna,
  claude → gpt, gpt → claude, gpt → deepseek2; every switched turn completed with tools. A route
  that later refuses a history shape turns that shape into a `validate` rejection.

## 4. Stage 3 — interactive (TUI owner, issue #12; line mode by the lead)

- `/model` opens the §4.6 picker of the TUI spec: groups by route, rows `E/P` with effort and
  credential source; unavailable (no credential) rows faint. Accept = `reconfigure`; a refusal
  is one transcript note with the reason. `/model REF` switches without the picker.
- `/effort LEVEL` switches effort only (a reconfigure with the same parts and new options).
- A cycle key steps through the scope (`enabled_models`, else every model) — pi's Ctrl+P, but
  `^P` pins the pane here (TUI SPEC §7); the TUI owner picks the key.
- Saving: picker action writes `default_model` / `enabled_models` to `settings.toml`
  (rewrites only those keys).
- Line mode (non-TUI interactive): `/model` prints the `p1 models` table, `/model REF` and
  `/effort LEVEL` switch; everything else stays as it is.

## 5. Out of scope

Choosing the model of a delegated worker (the delegate tool's own environment argument
stays); price display (no route publishes prices); model catalogues fetched from the network.
