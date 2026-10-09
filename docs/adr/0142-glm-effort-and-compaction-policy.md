---
adr: 142
title: GLM effort and compaction policy
status: accepted
date: 2026-10-09
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0142: GLM effort and compaction policy

## Context

The owner selected issue #637 R1 and R6 on 2026-10-09. Flash offered only high
based on an unconfirmed claim that other levels became max. The glm environment
kept a conservative 260000 window and summarized at 150000 while the coding
plan's capacity was unresolved. Lead measurements now support the documented
1M window on the unsuffixed Chat model. Extend ADR-0136's compaction rule to GLM.

## Decision

1. ClinePass GLM-5.3 Flash profile revision 2 offers low/high/max; default high
   remains unchanged. No route or thinking-flag changes.
2. Set glm window 1000000, reserve 131072 (the GLM-5.3 profile output ceiling).
   Summarize and trim together at min(0.8 * W, W - O - 65536):
   min(800000, 803392) = 800000. Retain ADR-0136's existing trim/re-measure/
   summarize sequence, not a new algorithm.
3. Adopt ZCode's summary cap 20000. Keep recent 50000 and user verbatim 8000:
   no evidence supports changing them. Keep wire model glm-5.3 and effort high.

## Consequences

Histories below 800000 tokens stay byte-exact rather than being rewritten at
150000, preserving cached prefixes longer. The input wall is 868928 and summary
pressure starts below it. Flash low and max are selectable, including low for
the existing summary-effort selection. Probe inputs demonstrate acceptance up
to 954796, not an exact ceiling or a speed improvement; effort measurements
are n=1 per level. Other environments and profiles retain their settings.

## Alternatives considered

Keep the conservative 260000 window: rejected after the lead's large-input
probes. Add [1m] to the wire model: rejected by this endpoint. Adopt ZCode's
966K compaction point: rejected because it leaves no output reserve in p1.
Other #637 rows, Flash output caps and thinking flags are separate work.

## Evidence

- [GLM-5.3 docs](https://docs.z.ai/guides/llm/glm-5.3): 1M context, 128K output.
- Lead probes 2026-10-09 on
  `https://api.z.ai/api/coding/paas/v4/chat/completions`, model `glm-5.3`:
  prompt_tokens 272754 / 600410 / 954796 all HTTP 200. `glm-5.3[1m]`:
  HTTP 400, code 1211. Only these reported measurements are recorded, not traffic.
- [Flash docs](https://docs.z.ai/guides/llm/glm-5.3-flash): accepted efforts
  low/high/max, default max. Lead ClinePass measurement 2026-10-09, release
  a250b52, route `cline-pass-1`, wire `cline-pass/glm-5.3-flash`, same prompt,
  one request per level: reasoning tokens low 39 / high 56 / max 97.
- ZCode v3.14.4 `resources/config/provider/zcode-builtin.json` around line 984,
  under `~/.agents/xo/dispatch/p1-lead-20261004/glm/harness-src/`: summary cap 20K.
- Offline inventories in `crates/p1-assembly/tests/context_table.rs` and
  `crates/p1-host/tests/models.rs`; built `p1 env show glm` and `p1 models`.
