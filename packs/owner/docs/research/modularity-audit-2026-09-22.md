# Modularity audit, 2026-09-22

The owner asked for an ultracode-style audit of p1 against its modularity goal, run by
DeepSeek V4.1 Flash workers, not Claude subagents. The lead (Claude) designed it with pane %39,
built `scripts/workflow.py` and `scripts/audits/modularity.py`, ran it, and wrote this synthesis.

- Commit audited: `1c93432`.
- Raw results (not in the repo): `../phaseone-briefs/modularity-audit/run2/`, with
  `result.json`, `facts.md`, one session journal per job under `fanout/runs/`, and every brief.
- Design: `../phaseone-briefs/modularity-audit/workflow.md`, including the amendments from %39's
  review.

## How it ran

| Stage | What | Size |
|---|---|---|
| Preflight | both auditor environments resolve (`p1 env show`) | — |
| Scout (no model) | crate graph, layering rules computed, `p1-contracts` usage matrix, swap cost, units derived from the tree and validated (every crate covered, ≤ 2600 lines per unit) | 41 units, 0 scripted violations |
| Find | one DeepSeek worker per unit × lens, in its own worktree at the pinned commit, reads checked in the session journal, quotes and repro commands re-run by the workflow | 47 units (41 + 6 from the critic) |
| Verify | two refuters per finding on `max` effort: a rule lens and a code lens; kept only if both uphold | 23 findings |
| Critic | coverage gaps → new Find units | 1 round, 6 units |

**Totals:**
- 104 p1 jobs, 5 of them repair rounds.
- About 11.9M uncached input tokens, 132M cache-read and 2.1M output. The route reports no cost.
- No job was voided for writing to its worktree.
- No job was left failed after the resumes.

## Verdict: the architecture holds

- **Layering is exact.** Scout's computed rules found no violation:
  - `p1-core` depends only on `p1-contracts`;
  - tools depend only on contracts and workspace;
  - providers depend only on contracts, model-profile and provider-http;
  - nothing other than host and assembly depends on a concrete tool or provider;
  - test support is never a normal dependency.

  The core-purity unit (`p1-core` read in full) produced no finding.
- **Swapping is cheap:**
  - a tool is named in production code only in `p1-host/src/catalog.rs`;
  - `finish` and `delegate` are also named in `run.rs` and `activity.rs`;
  - `p1-contracts/src/tool.rs` and `p1-workers` mention one only in doc comments;
  - a provider is named in production code only in `catalog.rs` and `routes.rs`.
- **The contract surface is cohesive.** Every `p1-contracts` item has at least 3 consumers.

What the audit did find: one broken design decision, one real defect caused by tool-name
coupling, and a set of copied helpers.

## Confirmed: both refuters upheld, and the lead re-checked

1. **`p1-tui` does terminal I/O, contrary to ADR-0043** (medium).
   - The ADR makes `p1-tui` a "pure state machine and cell renderer … no agent, no terminal".
   - `crates/p1-tui/src/runtime.rs:241-260` (`TerminalGuard`) enters the alternate screen and
     switches raw mode through crossterm; `crates/p1-tui/Cargo.toml` depends on crossterm.
   - Owner: the TUI session, issue #12. Either move the guard to `p1-host`, or write a new ADR
     if the terminal guard is meant to live in the UI crate.
2. **A copied sanitiser for provider error codes** (medium).
   - `safe_field` in `p1-provider-openai/src/parser.rs:268` and `safe_code` in
     `p1-provider-openai-chat/src/parser.rs` have byte-identical bodies. They are the sanitiser
     that keeps provider error codes safe to show (ADR-0048's "token-shaped code").
   - A security-relevant rule kept in two copies can drift. It belongs in `p1-provider-http`.

## Upheld by the lead from split votes (rule lens refuted, code lens upheld)

By design, a split medium or low finding is discarded. The lead re-read each split one.
- Finding 3 is a real defect.
- Findings 4–8 are small, real couplings worth one cleanup issue.
- The rest stay discarded as "allowed duplication".

3. **Hard-coded tool names in the UI and the host front end — a live defect** (medium).
   - `p1-host/src/tui.rs:490` (task-file tracking) and `p1-tui/src/transcript.rs:376-384` (call
     summaries) classify calls by the literal names `edit | patch | write | read | shell`.
   - They also decode those tools' private argument keys (`file_path`, `old_string`, `command`).
   - The patch tool is called `apply_patch` (`p1-tool-patch/src/lib.rs:25`), and its input is
     freeform, not JSON. So on the GPT environment neither place ever recognises a patch.
   - A renamed tool face (`ToolFace`) breaks every name match the same way.
   - The tool owns its declaration and argument semantics (seams.md §2). A display hint should
     come from the tool or its face — for example, the file a call targets — not from name
     matching in two other crates.
4. **A hidden global counter.**
   - `p1-host/src/run.rs:1838` `next_agent_ordinal` bumps a function-local `static` that feeds the
     worker cache-key scheme.
   - It is small, but it is exactly the kind of hidden global that AGENTS.md rules out. Pass the
     counter in explicitly.
5. **Credential variable-name validation copied into the host.**
   - `p1-host/src/routes.rs:161-167` repeats `p1-auth/src/spec.rs:105-111` verbatim, then calls
     `self.credential.validate()`, which runs the same check again.
   - Delete the host copy.
6. **`p1-workers` names a delegate-tool call.**
   - `p1-workers/src/lib.rs:4` says nothing there names a model-facing tool.
   - Yet line 598 writes "Use worker_result to read its result" into the parent's inbox.
   - Either pass the text in from `p1-tool-delegate`, or correct the doc comment.
7. **`ToolFace` defined three times.**
   - It lives in `p1-workspace`, `p1-tool-finish` and `p1-tool-delegate`.
   - `p1-host/src/catalog.rs` needs three matching macros (`apply_face!`, `apply_delegate_face!`,
     `apply_finish_face!`).
   - Its own documentation says it is "defined here so every tool module re-exports the same
     type". It is a contract-shaped type: move it to `p1-contracts`, or have finish and delegate
     re-export the workspace one.
8. **Host tests read the providers' private test trees.**
   - `p1-host/tests/{anthropic,openai}_route.rs` use `#[path]` to include
     `p1-provider-*/tests/fixtures/mod.rs`, and `route_files.rs` uses `include_str!` on nine
     fixture files.
   - Shared fixtures belong in `p1-provider-conformance`.

## Discarded (not a rule violation, or not true)

| Finding | Why it was discarded |
|---|---|
| Private `invalid()`, `identity()` and `default_face()` repeated in every tool crate; `invalid()` in every provider | Allowed duplication of a few lines (seams.md §1 bans only duplication forced by a dependency ban); merging would couple the siblings |
| `policy_name`, `http_error_code`, the status → error-kind table, `validate_composition`, route `origin()` repeated across providers | Same reason; each is 3–10 lines. `http_error_code` and the status table could join finding 2 in one "provider-http helpers" change |
| Read-before-mutate guard in both edit and write | Two call sites of one shared `p1-workspace` check, plus the spec-mandated messages |
| Output bound constants repeated in the tools | Refuted on the code: `bound_output` has no defaults; each tool picks its own limits |
| Summary-output default in both assembly and context | The context design places it there |
| Host's static header list | A five-name constant, used for validating route files |

## What the run taught about the workflow

**Three workflow bugs** surfaced in the run itself, and each was fixed and landed before the
resume:
- `1c93432`: p1 resolves profiles and routes next to the environments directory. The fix is a
  run-local share directory plus a preflight check.
- `d78f385`: a repair round needs the job's worktree, so trees now stay until the run ends.
- `0dc34c4`: repro commands are rejected only for *unquoted* shell operators.

The resume behaviour (a valid output is reused, and an invalid one is re-checked under the
current rules) meant none of these cost a re-run.

**DeepSeek as an auditor:**
- Every finding it reported had real quotes and a repro command that worked; the journal and
  quote checks rejected nothing that survived its repair round.
- It over-reports "duplication" as a rule violation.
- The rule-lens refuter was the useful brake: 17 of 21 discarded findings were refuted on the
  rule, with the facts conceded.

**Change for the next run:** a split vote on a *medium* finding should also get a third vote.
Finding 3 was a real defect that only the lead's second read recovered.

## Follow-up

| # | Action | Owner |
|---|---|---|
| 1 | TUI terminal I/O vs ADR-0043 | TUI session, #12 |
| 3 | A tool describes its own call target for display; drop name matching in `p1-tui` and `p1-host/src/tui.rs` (fixes `apply_patch` on GPT) | lead, new issue; the `p1-tui` part with #12 |
| 2 (+ table) | Error-code sanitiser, `http_error_code` and the status table move to `p1-provider-http` | new issue |
| 4–8 | Cleanups: explicit ordinal, host credential copy, workers doc, one `ToolFace`, fixtures in conformance | new issue |
| — | Workflow: third vote on split medium findings | lead, `scripts/audits/modularity.py` |
