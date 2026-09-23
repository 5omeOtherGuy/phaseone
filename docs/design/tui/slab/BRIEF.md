Design task: finish SLAB Harness so it covers the whole p1 TUI, and hand it off in a form a Rust (ratatui) implementer can build exactly

This project uses the SLAB Harness design system, which is authoritative for p1's terminal UI. The GitHub repo 5omeOtherGuy/phaseone is linked as the codebase. The attached files are 7 screenshots of the current TUI and the old spec. It already defines surfaces, ink roles, signal hues, glyphs, the three-band BLOCK, Decision, Picker, Worker, Composer, Statusline, Pane and a 120x40 session kit. It does NOT yet cover everything p1 has, needs now, or will have soon. Design those missing parts in the same language, then write an implementation handoff. Please use high reasoning: think through every state before drawing it.

## What p1 is (read the code — the repo is public)
- Repo: https://github.com/5omeOtherGuy/phaseone. `main` is the product; the TUI's newest work is on branch `task/12-tui`.
- p1 is a lean Rust coding-agent harness. One small core (the agent loop), and everything else is a module: providers, tools, journal, context policy, delegation, frontends.
- The TUI is `crates/p1-tui`, a pure state machine plus a cell renderer on ratatui. The terminal driver and the `FrontEnd` seam are in `crates/p1-host/src/tui.rs` (ADR-0043).
- Read these files:
  - `docs/design/tui/SPEC.md`: the old spec, still valid for keys, journal, access model and non-goals. It is also attached.
  - `crates/p1-tui/src/{state.rs,transcript.rs,input.rs,render/*}`: what the TUI actually draws today.
  - `crates/p1-contracts/src/policy.rs`: `AgentEvent`, `TurnEnd`. Everything a frontend can observe comes from here.
  - `crates/p1-host/src/tui.rs`: the slash commands, approval view and status groups.
  - `crates/p1-host/src/run.rs` (`/model`, `/effort`) and `STATUS.md`: current state and open work.
  - `docs/adr/`: ADRs 0036–0050 matter most. They cover context summaries, finish, access, model switching and workers.
- The attached screenshots are the current TUI at 120x40, rendered through the real renderer: idle, streaming, fold, diff, permission, picker, failure. It is still monochrome, from the old SPEC. SLAB Harness replaces that look.

## Hard constraints: design for a real terminal, not a web page
- Everything is a cell grid, with one monospace font and one glyph per cell. There are no half-cells, no pixel offsets, no font sizes or weights beyond bold, no opacity except the single ▪▪▪ working pulse, no images, and no borders or box-drawing (U+2500–257F).
- 24-bit colour is expected. Also specify the 256-colour and NO_COLOR degradation: glyphs plus layout alone must still read. P1_REDUCED_MOTION=1 freezes ▪▪▪.
- The minimum size is 80x24, the reference is 120x40, and the terminal can be wider (160–240 cols) or very short (≤12 rows). Resizing happens live.
- The transcript is append-only scrollback. Settled rows never change. Only the running block, the composer, the pane and the statusline update.
- Keep the existing rules: three surfaces, the ink roles, hue only on glyphs and outcome markers, one amber blocking event per view, facts not narration, `—` for unknown cost (never 0), `[h-xxxx]` fold handles, and caret key hints.

## Vocabulary correction: use p1's real names
p1's actual tools are `read`, `write`, `edit`, `apply_patch` (GPT routes, multi-file), `grep`, `shell`, `finish`, `worker_start`, `worker_continue`, `worker_result` and `worker_cancel`. There is no `search`, `skill`, `send`, `stop`, `ask`, `notify` or `compact` tool, and the DS's `delegate` is really the `worker_*` family. Issue #46 says tools will describe their own call target (name, target, outcome), so the UI must not hard-code tool names. Design one generic Block recipe that works for any future tool, plus the specific body rules for the tools above.

Environments and routes are `claude`, `deepseek`, `deepseek2`, `glm` and `gpt`. A model reference looks like `claude/opus-5.5:high`. Efforts are low, medium, high and max. Access is `full` by default; `--ask` opts into approvals (ADR-0038). The shell can run inside bubblewrap.

## Missing parts to design (every one needs a state and a mock)
1. **Responsive geometry.** Cover 80x24, 100x30, 120x40, 160x48 and ≤12 rows. Decide when the pane collapses, what the statusline drops first at narrow widths, and how the transcript width flexes; 76 is only the 120-col reference. The old spec had `^W` cycle the pane width (off/40/56/split) and `^L` force the pane as an overlay. Decide what survives.
2. **Home / first run / resume.** Today it shows a version line, `no journal in this directory.`, the /resume /env /access /goal affordances, and a dotted "p1" art logo that uses │ and ─, which break the no-box-drawing rule. Decide on the logo. Also design the `/resume` session list.
3. **Slash commands.** Live in the source today: `/model` (a picker over environments, models and effort, switching mid-session), `/model REF`, `/effort LEVEL`, `/goal`, `/focus` and `/exit`. Shown on the home screen but not wired yet: `/resume`, `/env` and `/access`. Planned: `/help` and a `/models` listing (the `p1 models` CLI exists). Design a command-completion popup on `/`, the model picker, the `/help` listing, and the goal editor.
4. **Streaming.** Cover the text stream, reasoning (collapsed `· reasoning 4.2s`, `^R` expand), a tool's arguments streaming before the call starts (`ToolInputDelta`), a request retrying after a transient provider failure (ADR-0041), a provider notice (for example a WebSocket→SSE fallback, ADR-0048), a context summary replacing history (`ContextReplaced`: items before and after, ADR-0036), and inbox messages delivered.
5. **Per-tool Blocks.** Specify the header, body, meta and fold rule for each real tool: read, write, edit, apply_patch (multi-file, `1 of 3 files`), grep, shell (tail kept, exit code, duration, sandboxed or not), finish (done / blocked / "not verified — parent verification required"), and each worker_* call. Include the generic fallback.
6. **Turn endings and errors.** Cover completed, cancelled (^C), provider failed (auth, exhausted account per ADR-0046, rate limit, network), journal commit failed, context failed, and a run stalled (ADR-0042). State what broke, what it cost, and what is still true. No banners.
7. **Approvals** (only under `--ask`). Cover the permission request (allow once / session / project / deny; ungrantable options shown with a reason, e.g. the destructive floor; the sandbox, network and cwd facts) and the blocking diff review (multi-file: next file, all files, ^D). Settle the relation between the DS Decision row and a full-width diff review.
8. **Workers (ADR-0050).** A main agent starts workers with explicit tool grants. Workers run on other routes in a bounded pool, report back, and can be continued (`worker_continue add_tools`) or cancelled. Design worker events inside the parent transcript, a WORKERS pane (states: queued, running, needs review, done, failed, cancelled, stalled, lost), attaching to a worker's own transcript (`a attach`, `x stop`, detach back), and worker cost when it is unknown.
9. **Pane modes.** LEDGER (context bar vs limit, parts: system/files/tools/recent, a warn threshold, spend in/out/cache hit/cost), OUTPUT (the fold viewer opened by `^O`, scrolling), DIFF (the session diff), and WORKERS. The DS pane shows SESSION/LEDGER/WORKERS/FOLDS. Reconcile the two into one pane model with mode switching (`^Tab`), and decide where GOAL and TASK go.
10. **Queueing.** While a turn runs, ⏎ queues a steering message and ⌥⏎ a follow-up. Show queued items (FAINT) and when they are delivered.
11. **Scrolling.** Show a scrolled-back position, a "new output below" affordance, and jumping back to the live tail.
12. **Statusline data mapping.** Map every field to real data: environment/route/model and effort (known), repo and branch (known), ctx % (known from usage when available), spend (often unknown → `—`), clock, and +/− lines (not tracked yet; say what it shows until it is). Show the full-width and 80-col versions.
13. **Focus mode** (old SPEC §4.3a). The pane hides, and the composer hides while empty. Restate it in SLAB.

Design for what p1 has now plus what is planned: workers, model switching, context summaries, finish, and the access model. Do not invent features p1 doesn't have (no MCP, no multi-session tabs, no mouse UI). Where you think something is needed but not planned, list it separately as a proposal.

## Deliverables (put them all in THIS project so they can be exported; the design system itself is synced from code, so new components live here first and I sync them into SLAB Harness afterwards)
1. New or updated components and guidelines in the existing style: Band-composed, `.jsx` + `.d.ts` + `.prompt.md`.
2. Updated `ui_kits/harness-session/`, and more kits if they help: home, narrow 80x24, workers, error.
3. **`handoff/TUI-HANDOFF.md`**: the document the Rust implementer builds from. It must be cell-exact:
   - For every screen or state, a plain-text grid mock at its real width (80 and 120 at minimum), with the role or colour of each span annotated (e.g. a legend of `[A]=BLOCK+ header`, `ink/dim/faint/ok/fail/attn/live/ref`).
   - Exact column stops, paddings, truncation and wrapping rules, and fold rules. Include which side truncates and where `…` goes.
   - A table mapping every p1 event and state to the element that renders it: `AgentEvent`, `TurnEnd`, approval, worker state.
   - The complete keybinding map, with context and conflicts.
   - The responsive rules as a decision table by width and height.
   - The colour tokens as hex, plus the 256-colour and NO_COLOR fallbacks.
   - Every place you intentionally depart from `docs/design/tui/SPEC.md`, with the reason.
   - Open questions for the owner, kept short.
4. When you are done, reply with a short list of the files you added or changed.
