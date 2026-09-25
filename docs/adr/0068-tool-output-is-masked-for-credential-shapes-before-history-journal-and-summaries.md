---
adr: 68
title: Tool output is masked for credential shapes before history, journal and summaries
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [crates/p1-redact/src/lib.rs, crates/p1-host/src/run.rs, crates/p1-context/src/lib.rs, crates/p1-tool-read/src/lib.rs, scripts/secret-scan.sh, scripts/test_secret_scan.py, scripts/gate.sh]
---
# ADR-0068: Tool output is masked for credential shapes before history, journal and summaries

## Context

Issue #142: credential-shaped strings must be caught before they reach anywhere they can
leak, and the CI gate must refuse a change that adds one. Two things can put a live
credential into a p1 conversation:

1. A tool's own output. `read`, `grep`, shell command output, and any future tool can
   return the contents of a file or the stdout of a command that happens to contain a
   provider API key, a bearer token, or a JSON credential field — for example a `.env`
   file, a curl invocation against an authenticated endpoint, or a stray `cat` of an auth
   file the tool was not specifically told to refuse. That text becomes a
   `ToolResultItem` in conversation history, gets written to the on-disk journal, and can
   be pulled into a context summary that the model itself later reads and may echo back.
2. A file `p1-tool-read` should simply never open at all: the p1 auth store, the Codex,
   Claude Code, opencode and Pi agent login files, and `~/.config/keys/**`. These hold the
   raw credential a route resolves — reading them at all is a bigger problem than masking
   their content afterward.

Neither the journal nor the context summarizer previously touched a tool's returned text
for shape; whatever a tool produced went into history and onto disk unmodified. The
canonical credential shapes to catch were fixed for the whole fleet in PR #51:
`sk-` keys (bare and the `ant`/`proj`/`or`/`svcacct`/`admin` families, all requiring 20+
trailing characters), so that the Rust matcher, `grep -E` and this ADR's bash gate script
all recognize the same strings. A CI gate check (`scripts/secret-scan.sh`) closes the gap
for source and fixture files; this ADR is about the runtime path, where a *legitimate*
tool call can return a credential-shaped string that was never typed into a file.

## Decision

1. **A new internal crate, `crates/p1-redact`,** depending only on `p1-contracts` and
   `regex` (no host, no provider, no storage dependency), provides:
   - a matcher function recognizing three shapes: `sk-…` keys (the PR #51 pattern), the
     token following `Authorization:` or `Bearer ` in a line, and any JSON string value
     whose key is `key`, `access`, `refresh`, `api_key` or `token` and whose value is 16
     or more characters;
   - a `redact(text: &str) -> Redaction` function returning the masked text and the count
     of replacements; every match becomes `<redacted:family:N chars>`, keeping only the
     family prefix — `sk-`, `sk-ant-` (and the other `sk-` modifier families), `Bearer`,
     `Authorization`, or the JSON key name — and the masked value's length. Never a
     character of the secret itself;
   - a `RedactingTool` decorator implementing `p1_contracts::Tool` that wraps any other
     tool and passes its `ToolOutcome.content` through `redact` before returning it.
2. **The host wraps every assembled tool.** `crates/p1-host/src/run.rs`,
   `assemble_with_cache_key`, wraps each tool the catalog assembles in `RedactingTool`
   before it is handed to p1-core. p1-core therefore always builds its `ToolResultItem`
   from already-masked content: history, the on-disk journal, and anything later derived
   from history (context summaries, transcripts, `p1 run --report`) only ever hold the
   masked form. p1-core's own API is unchanged — it still calls `Tool::execute` and gets a
   `ToolOutcome`; it has no redaction logic of its own and no new dependency.
3. **The context summarizer's own output is masked the same way.** `crates/p1-context`
   passes whatever the summarizer produces through `p1_redact::redact` before it becomes
   a history item, so a summary cannot reintroduce a credential that a masked tool result
   happened to still spell out across a paraphrase boundary.
4. **A per-turn redaction count is a display-only notice, never a value.** Each agent has
   one `p1_redact::MaskCounter`; the tools it assembles add to it, and the host wraps the
   agent's event sink so that at the turn boundary a non-zero count becomes an
   `AgentEvent::ProviderNotice { text: "masked N credential-shaped value(s) in tool
   output" }` — only the count, never the masked text, and with no effect on control flow.
5. **`crates/p1-tool-read` refuses a fixed deny-list before it opens a path**, independent
   of masking: the p1 auth store, `~/.config/keys/**`, `~/.codex/auth.json`,
   `~/.claude/.credentials.json`, `~/.local/share/opencode/auth.json` and
   `~/.pi/agent/auth.json`. This is a second layer — the tool declines to run at all — for
   the specific files known to hold a raw grant, rather than relying on the shape matcher
   alone to catch everything inside them.
6. **`scripts/secret-scan.sh` closes the source/fixture gap.** It walks `git ls-files` and
   `grep -nIE`s every tracked file for the `sk-` half of the PR #51 pattern (rule 2 of the
   lead's decision: the bash gate checks the `sk-` shapes only, since `Authorization:` and
   JSON-key matches are common enough in legitimate source and test fixtures to make a
   source-wide scan for them too noisy to keep green). It prints only `file:line` on a
   hit, never the matched text, and is wired into `scripts/gate.sh` right after core
   isolation.

## Consequences

- A tool result that happens to contain a live key is masked before anything durable
  (history, journal, summary) is built from it; only the in-memory `ToolOutcome` the
  redacting wrapper receives ever holds the raw text, and it is discarded once wrapped.
- p1-core and p1-contracts keep their existing public APIs: `p1-redact` is a decorator
  layered by the host at assembly time, not a change to the `Tool` trait or to
  `ToolOutcome`'s shape.
- An operator sees that masking happened (the notice) without ever seeing what was
  masked, matching the existing `AgentEvent::ProviderNotice` pattern used for other
  provider-facing, display-only information (ADR-0048).
- The deny-list in `p1-tool-read` means some legitimate diagnostic use (e.g. "cat my auth
  file to check its shape") is refused outright rather than masked; that trade favors
  refusing outright over risking an unmasked family the matcher has not been taught yet.
- `scripts/secret-scan.sh` only catches the `sk-` shapes in tracked files; a bearer token
  or JSON credential field typed directly into a committed file is not caught by the gate
  script (though it would still be masked at runtime if it ever passed through a tool
  result). This is accepted per rule 2 above rather than tuning the bash pattern to match
  the full three-family Rust matcher, which would need JSON-awareness bash does not have.
- Every new masking point is additive: a route, tool or context path that already worked
  keeps working; the only observable difference is that credential-shaped substrings in
  tool output are replaced by a fixed placeholder wherever they used to be verbatim.

## Alternatives considered

- **Mask only at the UI/render boundary, keep raw text in history and the journal.**
  Rejected: the journal is written to disk and can be shared for debugging or a run
  report; a credential surviving there defeats the point. The mask has to happen before
  the value becomes durable, not just before it is displayed.
- **Give `p1-core` its own redaction call instead of a host-side tool decorator.**
  Rejected: p1-core's isolation from provider/tool/storage crates (ADR checked by
  `scripts/check-core-isolation.sh`) would be broken by a new dependency on `regex` or on
  `p1-redact` inside p1-core. A decorator applied by the host before results reach
  p1-core keeps p1-core exactly as isolated as before.
- **Extend the bash gate script to the full three-family pattern (`sk-`, `Authorization:`,
  JSON keys).** Rejected for now: bash/grep has no JSON awareness, so the JSON-key family
  would need a much looser regex that flags ordinary source code (any `"token": "..."`
  fixture of 16+ characters) far too often to keep the gate usable. The lead's decision
  (rule 2) scopes the bash scan to the `sk-` shapes and leaves the other two families to
  the Rust matcher at runtime.
- **Redact at the provider-client boundary instead of decorating each tool.** Rejected:
  the leak this ADR targets originates in *tool* output (files, command stdout), not in
  what the model or provider sends; wrapping tools is the boundary closest to the source
  of the data.
- **Report the per-turn count as a return value or history item instead of a notice.**
  Rejected: a value that could itself be inspected or logged risks becoming another
  channel for the same class of leak (e.g. correlating a count with a specific request);
  a display-only, ledger-free notice avoids that.

## Evidence

- `crates/p1-redact/src/lib.rs` implements the matcher (`sk-…` per PR #51, `Authorization:`
  / `Bearer ` tokens, and 16+ character JSON values under `key`/`access`/`refresh`/
  `api_key`/`token`), the `<redacted:family:N chars>` mask, and the `RedactingTool`
  decorator.
- `crates/p1-host/src/run.rs`, `assemble_with_cache_key`, wraps every assembled tool in
  `RedactingTool` so `ToolOutcome.content` reaching p1-core is already masked.
- `crates/p1-context/src/lib.rs` passes the summarizer's output through the same
  `p1_redact::redact` function before it becomes a history item.
- `crates/p1-tool-read/src/lib.rs` refuses the p1 auth store, `~/.config/keys/**`,
  `~/.codex/auth.json`, `~/.claude/.credentials.json`,
  `~/.local/share/opencode/auth.json` and `~/.pi/agent/auth.json`.
- `scripts/secret-scan.sh` scans `git ls-files` for the `sk-` shapes and is wired into
  `scripts/gate.sh` right after `== gate: core isolation`; `bash scripts/secret-scan.sh`
  passes on a clean tree, and `shellcheck scripts/secret-scan.sh` is clean.
  `python3 scripts/test_secret_scan.py -q` passes, exercising the bare `sk-`, `sk-ant-`
  and `sk-proj-` families plus a below-threshold negative control, every test vector
  built at runtime (`"sk-" + "a" * 24` and similar) rather than written as a literal.
- `python3 scripts/adr.py check` passes (ADR-0066 and ADR-0067 merged first).
