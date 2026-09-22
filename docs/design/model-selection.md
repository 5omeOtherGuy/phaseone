# Model selection and scoped models

ADR-0049 (owner request 2026-09-21: "we are missing basic functionality. Model selection /
scoped models like in pi"). Donor for the behaviour, not the code: pi (`docs/usage.md`,
`settings.md`, `keybindings.md` of `@earendil-works/pi-coding-agent`): `--model`, `--thinking`,
`--models <patterns>` for cycling, `--list-models`, `/model`, `/scoped-models`, `enabledModels`.

## 1. What a model is in p1

An **environment** fixes the prompt family, the tools and `[context]`; its **route** × **profile**
fixes the provider (ADR-0039). A *model* the operator selects is therefore a pair:

```
<environment>/<profile>[:<effort>]        e.g.  gpt/gpt-5.6-luna:high
```

The candidates are exactly: every environment E, every profile P bound in E's route file
(`[models."P"]`). Every main agent has the worker tools (ADR-0050), so delegation is no longer an
environment property: a model is listed once per environment, and `claude/claude-opus-5` is one
model, not two. Nothing else is selectable; there is no free-text model id.

**Reference resolution** (`--model`, `/model`, `default_model`), in order:
1. `E/P` — that pair; unknown pair → error listing the pairs whose P or E matches.
2. bare `P` — the pairs whose profile is P. If the CURRENT environment (the `--env` given, else
   the default) is among them, that one; else if exactly one, that one; else an error that lists
   them as `E/P` (never a guess).
3. `:effort` — must be one of the profile's `efforts`; absent keeps the environment's
   `[options] reasoning_effort` if the profile supports it, else the profile default.

## 2. Stage 1 — choose at start (no core change)

- `--model REF` (with or without `--env`; together they must agree, else a usage error).
  `--effort LEVEL` overrides the effort of whatever was selected. Both work headless.
- `p1 models [SEARCH]` prints one row per model, sorted by environment then profile:
  `E/P`, route id, efforts (`low…max` as the profile lists them, `-` if none), credential source
  (the same wording as `p1 env show`'s `credential` line — never a value), and `default` /
  `scoped` markers. SEARCH is a case-insensitive substring filter on `E/P`.
- `~/.config/p1/settings.toml` (XDG like the p1 store; absent = empty; unknown key = error
  naming the file and key):
  ```toml
  default_model  = "claude/claude-sonnet-5"      # replaces DEFAULT_ENV when set
  enabled_models = ["claude/*", "gpt/gpt-5.6-sol*", "deepseek2/*"]   # the scope
  ```
- `--models PATTERNS` (comma-separated) replaces `enabled_models` for this run. A pattern is a
  glob (`*`, `?`) matched against `E/P`; a pattern without `/` matches the profile part only.
  An empty scope means "every model". A pattern that matches nothing is an error (a typo must
  not silently shrink the scope).
- `p1 env show` gains the resolved `model  E/P:effort` line.

## 3. Stage 2 — switch within a session (core, supersedes ADR-0033)

- Core: `Agent::reconfigure(Reconfiguration { provider, tools, system_prompt, options, context })`
  — callable only between turns (`&mut self`). It runs exactly `assemble`'s checks (duplicate
  tool names; `provider.validate`) but validates against the CURRENT history, not an empty one.
  On success the next turn commits a new `Environment` record before its input (the existing
  `environment_committed = false` path; journal.md already says "again whenever the environment
  is explicitly changed"). On failure nothing changes and the error names the reason.
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
