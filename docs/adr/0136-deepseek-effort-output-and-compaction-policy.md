---
adr: 136
title: DeepSeek effort output and compaction policy
status: accepted
date: 2026-10-08
deciders: owner
supersedes: [127]
superseded_by: []
sources: []
---
# ADR-0136: DeepSeek effort output and compaction policy

## Context

Issue #622 compares DeepSeek's API and dsh with p1. The owner selected R1, R2 and
R4 on 2026-10-08; the lead's implementation brief pins the values below. Earlier
DeepSeek configuration claimed no profile capacity, omitted a response cap and
trimmed at 150000, well before dsh's compaction pressure. This reverses ADR-0127's
strictly earlier trim threshold and shipped DeepSeek values, not its trimming algorithm.

## Decision

1. DeepSeek V4.1 Flash supports low/high/max effort, default high. State model
   context 1000000 and conservative output ceiling 384000 in profile revision 2.
   DeepSeek docs state 384K (393216); retain the smaller gateway metadata ceiling
   requested by the owner, not a claim that the official limit is 384000.
2. Existing DeepSeek/Cline environments send max_output_tokens 256000 (Chat wire
   max_tokens) and reserve the same amount. Summary and trim trigger together at
   min(0.8 * 1000000, 1000000 - 256000 - 65536) = 678464. Keep recent 119040
   tokens (16% of the 744000 message budget), summary cap 65536. User budget unchanged.
3. Allow positive trim_at_tokens at or below summarize_at_tokens. Keep the existing
   trim, re-measure, summarize-if-still-above sequence; insufficient trim summarizes
   the original history, whose summary renderer already excerpts results. No algorithm
   or pruning-shape changes. Other environments retain their settings.
4. Defer reasoning off: ADR-0129 exposes it only on Responses, whereas Chat has no
   off option and the shared Effort enum has no off variant. Do not treat omission as off.

## Consequences

Histories below the selected pressure remain byte-exact, avoiding premature prefix
rewrites. Low becomes available without changing agent defaults; existing summary
effort selection now uses low. More headroom and a larger summary cap leave room for
reasoning-heavy responses. Token counts are policy/source values, not measured speed gains.
Cline's capacity remains model-documented rather than a measured gateway limit.
The existing fallback summarizes original history after insufficient pruning; dsh
continues with its pruned history. This known mismatch is not changed in this slice.

## Alternatives considered

Keep 150000 trim / 300000 summary: rejected by the selected R4 policy. Change every
provider's context defaults or add an off enum: outside selected rows. Change the
summary fallback to consume pruned history: not required by the brief, which asks
to record the mismatch; existing behavior retained.

## Evidence

- [DeepSeek Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode),
  Thinking Mode Toggle and Effort Control: low/high/max, default high.
- [Models & Pricing](https://api-docs.deepseek.com/quick_start/pricing/), Model Details:
  1M context / 384K output; [Chat request](https://api-docs.deepseek.com/api/create-chat-completion),
  max_tokens: numeric ceiling 393216. Gateway metadata documented in
  `docs/design/context-windows.md` states output 384000.
- Installed `@deepseek-ai/dsh@0.2.0-rc.2`:
  `~/.local/share/dsh/node_modules/@deepseek-ai/dsh-llm-deepseek/lib/index.js:17-21`
  defaults context/output; `dsh-compaction-basic/lib/index.js:63-145` resolves
  headroom, pressure and retention; `:926-946` prune before summary.
- Offline tests in `p1-assembly/tests/context_table.rs`, `p1-context/src/engine.rs`
  and `p1-context/tests/component.rs` check equality and its boundaries, native/Wasm
  parity, no trim below pressure and no summary when trimming suffices. Host context
  tests check profile folding and summary cap; shipped environment tests pin all seven presets.
