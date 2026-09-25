// p1-screens — every state of the p1 TUI as data: component-level mocks (`elements`) and
// full screens (`screens`). Rendered by lib/p1-cells.js; the handoff grids are generated from
// this file, the visual kit renders it. Model/route names follow the repo's environments and
// profiles; numbers are illustrative.
(function () {
  var g = typeof window !== "undefined" ? window : globalThis;
  var P = g.P1, u = P.util, C = g.P1Cells;
  function c(name, props) { return function (W, ctx) { return P[name].rows(Object.assign({ width: W }, typeof props === "function" ? props(W, ctx) : props)); }; }
  function evs(list) { return function (W, ctx) { return u.events(list.map(function (f) { return f(W, ctx); }), W); }; }
  function cat(list) { return function (W, ctx) { var o = []; list.forEach(function (f) { o = o.concat(f(W, ctx)); }); return o; }; }

  // ---------- shared data ----------
  var ME = "claude/opus-5.5";
  var EDGE = "crates/p1-context/src/edge.rs";
  var EDIT_DIFF = [
    { diff: "ctx", line: 411, text: "let pressure = self.pressure_at_edge();" },
    { diff: "del", line: 412, text: "if pressure == Pressure::Hard {" },
    { diff: "del", line: 413, text: "    block_until_ready(&worker);" },
    { diff: "del", line: 414, text: "}" },
    { diff: "add", line: 412, text: "if let Some(summary) = ready {" },
    { diff: "add", line: 413, text: "    return self.apply_at_boundary(summary);" },
    { diff: "add", line: 414, text: "}" },
    { diff: "ctx", line: 415, text: "self.commit_boundary()" }
  ];
  var SHELL_TAIL = ["test compaction::case_7 ... ok", "failures:", "", "---- compaction::hard_pressure_waits stdout ----",
    "thread 'compaction::hard_pressure_waits' panicked at crates/p1-context/src/edge.rs:414:9:", "assertion `left == right` failed", "  left: Hard", " right: Ready"];
  var COMP = {
    idle: { placeholder: "message, / for commands", hint: "\u23ce send   \u2325\u23ce newline", secondary: "^C quit" },
    working: { placeholder: "steer the running turn", hint: "\u23ce queue steering   \u2325\u23ce queue follow-up", secondary: "^C cancel" },
    approval: { disabled: true, placeholder: "decide above", hint: "", secondary: "^C cancel turn" },
    attached: { disabled: true, placeholder: "attached to w2 \u2014 read only", hint: "esc detach   x stop", secondary: "PgUp scroll" },
    menu: { hint: "tab complete   \u23ce run", secondary: "esc dismiss" }
  };
  function comp(state, extra) { return function (W, o) { return P.ComposerBar.rows(Object.assign({ width: W, compact: o && o.compact }, COMP[state], extra || {})); }; }
  function status(extra) { return c("StatusBar", Object.assign({ model: ME, effort: "high", repo: "phaseone", branch: "main", ctx: "10%", spend: "\u2014", clock: "0h14" }, extra || {})); }
  var LEDGER = { goal: "fix compaction boundary stall", session: { model: ME, effort: "high", access: "full", sandbox: "bubblewrap" },
    context: { used: "12.4k", window: "120k", pct: 10, summarizeAt: "96k" }, workspace: { files: "1", diff: "\u2014", journal: "12s ago" },
    spend: { "in": "38.1k", out: "1.9k", hit: "16%", cost: "\u2014" }, folds: [["h-0275b8a9", "shell \u00b7 94 lines"]] };
  function ledger(extra) { return function (W, H) { return P.LedgerPane.rows(Object.assign({ width: W, height: H }, LEDGER, extra || {})); }; }
  function strip(current, modes) { return c("PaneStrip", { modes: modes || ["ledger", "output", "workers"], current: current || "ledger" }); }

  var WORKERS = [
    { id: "w3", task: "audit sandbox read paths", route: "deepseek2/v4.1-flash", model: "deepseek-v4.1-flash", tokens: 12400, state: "review", elapsed: "0m48s", grants: "read grep shell finish", activity: "shell rm -rf target/ \u00b7 awaiting approval" },
    { id: "w2", task: "split provider-http helpers", route: "deepseek2/v4.1-flash", model: "deepseek-v4.1-flash", tokens: 48213, ctx: 128000, state: "running", elapsed: "0m52s", grants: "read edit shell finish", activity: "edit crates/p1-provider-http/src/retry.rs" },
    { id: "w4", task: "measure summarize threshold", route: "glm/5.3", model: "glm-5.3", tokens: 3100, state: "failed", elapsed: "1m03s", grants: "read shell finish", activity: "RateLimited: HTTP 429" },
    { id: "w6", task: "rename ToolFace", route: "deepseek/v4.1-flash", state: "stalled", elapsed: "6m40s", grants: "read edit finish", activity: "6 summaries without a workspace change" },
    { id: "w5", task: "doc note for ADR-0050", route: "claude/sonnet-5", state: "queued", grants: "read write finish", activity: "waiting for a pool slot" },
    { id: "w1", task: "reject cred-dir ancestors", route: "gpt/gpt-5.6-luna", state: "unverified", elapsed: "2m10s", grants: "read edit finish", activity: "not verified \u2014 parent verification required" },
    { id: "w0", task: "resume probe", route: "claude/opus-5.5", state: "lost", grants: "read finish", activity: "not restored on resume" }
  ];

  // ---------- transcript fragments ----------
  var F = {
    ask: c("OperatorTurn", { text: "why does compaction stall at the turn edge?" }),
    reason: c("Reasoning", { elapsed: "4.2s" }),
    prose: c("ProseFlow", { lines: [[["ink", "The hard-pressure wait in "], ["ref", "crates/p1-context/src/edge.rs"], ["ink", " blocks the turn boundary instead of applying the summary the worker already prepared. Three things line up:"]]] }),
    read: c("ToolCall", { name: "read", target: EDGE, targetTone: "ref", status: "ok", outcome: "412 lines \u00b7 14.2 kB" }),
    grep: c("ToolCall", { name: "grep", target: "block_until_ready crates/", status: "ok", outcome: "3 hits \u00b7 2 files" }),
    edit: c("ToolCall", { name: "edit", target: EDGE, targetTone: "ref", status: "ok", outcome: "+3 \u22123", body: EDIT_DIFF }),
    shellRun: c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", state: "running", elapsed: "4.2s" }),
    shellFail: c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", status: "fail", outcome: "11.4s \u00b7 exit 101 \u00b7 94 lines", body: SHELL_TAIL,
      meta: { left: [["dim", "cwd ~/dev/phaseone \u00b7 bubblewrap \u00b7 writes: workspace \u00b7 net off"]] }, fold: { hidden: 86, earlier: true, handle: "h-0275b8a9" } }),
    confirmed: c("ProseFlow", { lines: ["Confirmed \u2014 the ready summary never applies. Fixing the boundary and re-running."] })
  };
  var SESSION = [F.ask, F.reason, F.prose, F.read, F.grep, F.edit, F.shellRun];

  // ---------- component-level mocks ----------
  var elements = [
    { id: "el-operator", title: "OperatorTurn \u2014 wrapped prompt; steering delivered mid-turn", widths: [76, 56], rows: evs([
      c("OperatorTurn", { text: "the compaction boundary stalls when the worker already has a summary ready; find where the hard-pressure wait blocks and fix it without changing the summary format" }),
      c("OperatorTurn", { text: "use the existing apply_at_boundary helper", tag: "steering" })]) },
    { id: "el-reasoning", title: "Reasoning \u2014 collapsed, then expanded (^R)", widths: [76], rows: evs([
      c("Reasoning", { elapsed: "4.2s" }),
      c("Reasoning", { elapsed: "4.2s", expanded: true, lines: ["The wait only exists for the no-summary case. If a summary is ready the boundary can apply it directly; the hard-pressure branch predates the worker summary path."] })]) },
    { id: "el-working", title: "TurnWorking \u2014 waiting, reasoning, streaming, preparing, summarizing", widths: [76], rows: evs([
      c("TurnWorking", { phase: "waiting", elapsed: "1.2s", request: 1 }), c("TurnWorking", { phase: "reasoning", elapsed: "3.0s", request: 1 }),
      c("TurnWorking", { phase: "streaming", elapsed: "6.0s", request: 3 }), c("TurnWorking", { phase: "preparing", elapsed: "2.4s", request: 3 }),
      c("TurnWorking", { phase: "summarizing context", elapsed: "8.1s", request: 9 })]) },
    { id: "el-meta", title: "MetaRow \u2014 provider notice, context replaced, model switch, goal, inbox, unknown command, lost workers", widths: [76], rows: evs([
      c("MetaRow", { text: "transport: WebSocket unavailable (426) \u2014 using HTTP (SSE) for the rest of this session" }),
      c("MetaRow", { text: "context summarized \u00b7 214 \u2192 31 items", right: [["dim", "in 18.2k \u00b7 out 1.1k"]] }),
      c("MetaRow", { text: "model claude/opus-5.5:high \u2192 gpt/gpt-5.6-sol:medium \u00b7 from the next turn" }),
      c("MetaRow", { text: "goal set" }), c("MetaRow", { text: "1 inbox message delivered" }),
      c("MetaRow", { text: "/env is not a command \u00b7 /help" }), c("MetaRow", { text: "2 workers not restored on resume" })]) },
    { id: "el-monogram", title: "Monogram \u2014 slab mark (BLOCK+ cells, no glyphs)", widths: [76], rows: c("Monogram", {}) },
    { id: "el-tool-states", title: "ToolCall \u2014 generic recipe in every state", widths: [76], rows: evs([
      c("ToolCall", { name: "", target: "*** Begin Patch", state: "preparing", size: "1.4 kB", body: ["+    return self.apply_at_boundary(summary);", "+}", " self.commit_boundary()"] }),
      c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", state: "running", elapsed: "4.2s" }),
      c("ToolCall", { name: "shell", target: "cargo build --release", state: "awaiting", outcome: "awaiting approval" }),
      c("ToolCall", { name: "read", target: EDGE, targetTone: "ref", status: "ok", outcome: "412 lines \u00b7 14.2 kB" }),
      c("ToolCall", { name: "lint", target: "crates/p1-tui", status: "fail", outcome: "2.1s \u00b7 3 findings", body: ["warning: unused variable `pad` at render/diff.rs:131", "warning: needless borrow at render/ledger.rs:88", "error: this `if` has identical blocks at state.rs:412"] }),
      c("ToolCall", { name: "shell", target: "rm -rf target/", status: "denied", outcome: "by operator" }),
      c("ToolCall", { name: "shell", target: "cargo test --workspace", status: "cancelled" }),
      c("ToolCall", { name: "read", target: "crates/p1-host/src/run.rs", targetTone: "ref", status: "unknown" }),
      c("ToolCall", { name: "search", target: "{\"pattern\":\"retries\"}", status: "unavailable" }),
      c("ToolCall", { name: "worker_continue", target: "w1 +edit", status: "ok", outcome: "resumed \u00b7 +edit" })]) },
    { id: "el-tool-states-56", title: "ToolCall at 56 (100-col transcript) \u2014 target truncates, outcome survives", widths: [56], rows: evs([
      c("ToolCall", { name: "read", target: EDGE, targetTone: "ref", status: "ok", outcome: "412 lines \u00b7 14.2 kB" }),
      c("ToolCall", { name: "shell", target: "cargo test -p p1-provider-http --all-features -- --nocapture", status: "ok", outcome: "3.1s \u00b7 exit 0 \u00b7 212 lines" }),
      c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", state: "running", elapsed: "4.2s" }),
      c("ToolCall", { name: "apply_patch", target: "3 files", status: "ok", outcome: "+48 \u221212 \u00b7 3 files" })]) },
    { id: "el-tools", title: "Per-tool Blocks \u2014 ok", widths: [76], rows: evs([
      c("ToolCall", { name: "read", target: EDGE + ":380-460", targetTone: "ref", status: "ok", outcome: "81 lines \u00b7 3.0 kB" }),
      c("ToolCall", { name: "write", target: "docs/design/tui/NOTES.md", targetTone: "ref", status: "ok", outcome: "48 lines \u00b7 1.9 kB \u00b7 new" }),
      c("ToolCall", { name: "edit", target: EDGE, targetTone: "ref", status: "ok", outcome: "+3 \u22123", body: EDIT_DIFF }),
      c("ToolCall", { name: "edit", target: "crates/p1-context/src/lib.rs", targetTone: "ref", status: "ok", outcome: "+21 \u22124", body: EDIT_DIFF.slice(0, 8), fold: { hidden: 17, unit: "diff rows", handle: "h-9c1e44d0" } }),
      c("ToolCall", { name: "apply_patch", target: "3 files", status: "ok", outcome: "+48 \u221212 \u00b7 3 files", body: [{ file: EDGE, right: "+12 \u22123" }, { file: "crates/p1-context/src/lib.rs", right: "+30 \u22129" }, { file: "crates/p1-context/tests/boundary.rs", right: "+6 \u22120" }] }),
      c("ToolCall", { name: "grep", target: "block_until_ready crates/", status: "ok", outcome: "3 hits \u00b7 2 files" }),
      c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", status: "ok", outcome: "3.1s \u00b7 exit 0 \u00b7 94 lines" }),
      c("ToolCall", { name: "finish", target: "done", status: "ok", outcome: "verified \u00b7 1 command", body: [[["ok", "\u2713 "], ["ink", "cargo test -p p1-context"], ["dim", "  3.1s \u00b7 after the last change"]]] })]) },
    { id: "el-tools-fail", title: "Per-tool Blocks \u2014 not ok (shell keeps its tail)", widths: [76], rows: evs([
      F.shellFail,
      c("ToolCall", { name: "read", target: "crates/p1-context/src/boundary.rs", targetTone: "ref", status: "fail", outcome: "no such file" }),
      c("ToolCall", { name: "edit", target: EDGE, targetTone: "ref", status: "fail", outcome: "old_string not found", body: ["old_string not found in crates/p1-context/src/edge.rs (read it again before editing)"] }),
      c("ToolCall", { name: "finish", target: "done", status: "fail", outcome: "rejected", body: ["no successful run of \"cargo test -p p1-context\" after the last change"] }),
      c("ToolCall", { name: "finish", target: "blocked", status: "blocked", outcome: "blocked \u00b7 needs edit", body: ["the worker was granted read, grep, finish; the fix needs edit"] }),
      c("ToolCall", { name: "finish", target: "done", status: "ok", outcome: "done \u00b7 not verified \u2014 parent verification required" })]) },
    { id: "el-workers-tools", title: "Worker tool Blocks in the parent transcript", widths: [76], rows: evs([
      c("ToolCall", { name: "worker_start", target: "w2 \u00b7 deepseek2/v4.1-flash", status: "ok", outcome: "started", body: [[["ink", "split provider-http helpers into p1-provider-http (#47)"]], { kv: ["grants", "read edit shell finish"] }] }),
      c("ToolCall", { name: "worker_start", target: "w7 \u00b7 glm/5.3", status: "fail", outcome: "not started", body: ["apply_patch is freeform; the glm route has function tools only"] }),
      c("ToolCall", { name: "worker_continue", target: "w1 +edit", status: "ok", outcome: "resumed \u00b7 +edit" }),
      c("ToolCall", { name: "worker_result", target: "w1", status: "ok", outcome: "blocked \u00b7 6 lines", body: [{ kv: ["tools", "read grep finish"] }, { kv: ["finish", "blocked"] }, { kv: ["needs", "edit"] }, { kv: ["tried", "edit \u00d72"] }] }),
      c("ToolCall", { name: "worker_cancel", target: "w4", status: "ok", outcome: "cancelled" })]) },
    { id: "el-worker-report", title: "WorkerReport \u2014 the host's line for every worker end", widths: [76], rows: evs([
      c("WorkerReport", { id: "w2", route: "deepseek2/v4.1-flash", state: "done", elapsed: "2m10s", grants: ["read", "edit", "shell", "finish"], line: "done \u00b7 verified \u00b7 cargo test -p p1-provider-http" }),
      c("WorkerReport", { id: "w1", route: "claude/sonnet-5", state: "blocked", elapsed: "0m41s", grants: ["read", "grep", "finish"], line: "blocked: needs edit \u2014 tried edit \u00d72" }),
      c("WorkerReport", { id: "w5", route: "gpt/gpt-5.6-luna", state: "unverified", elapsed: "1m12s", grants: ["read", "edit", "finish"], line: "done \u00b7 not verified \u2014 parent verification required" }),
      c("WorkerReport", { id: "w4", route: "glm/5.3", state: "failed", elapsed: "1m03s", grants: ["read", "shell", "finish"], line: "failed: RateLimited \u00b7 HTTP 429" }),
      c("WorkerReport", { id: "w6", route: "deepseek/v4.1-flash", state: "stalled", elapsed: "6m40s", grants: ["read", "edit", "finish"], line: "stalled: 6 summaries without a workspace change" })]) },
    { id: "el-approval-permission", title: "Approval (--ask) \u2014 permission, inline", widths: [76], rows: c("ToolCall", { name: "shell", target: "cargo build --release", state: "awaiting", outcome: "awaiting approval",
      body: [{ kv: ["cwd", "~/dev/phaseone", "ref"] }, { kv: ["sandbox", "bubblewrap \u00b7 writes: workspace"] }, { kv: ["network", "off"] }, { kv: ["effect", "runs a process"] }],
      decision: { options: [{ key: "y", label: "allow once" }, { key: "a", label: "session" }, { key: "p", label: "project", reason: "not available \u2014 no trust store yet" }, { key: "n", label: "deny" }] } }) },
    { id: "el-approval-floor", title: "Approval \u2014 destructive floor, from a worker, second request queued", widths: [76], rows: c("ToolCall", { name: "shell", target: "rm -rf target/", state: "awaiting", outcome: "awaiting approval",
      body: [{ kv: ["from", "w3 \u00b7 deepseek2/v4.1-flash"] }, { kv: ["cwd", "~/dev/phaseone", "ref"] }, { kv: ["sandbox", "bubblewrap \u00b7 writes: workspace"] }, { kv: ["network", "off"] }, { kv: ["effect", "runs a process \u00b7 destructive"] }],
      decision: { options: [{ key: "y", label: "allow once" }, { key: "a", label: "session", reason: "not grantable \u2014 destructive floor" }, { key: "p", label: "project", reason: "not grantable \u2014 destructive floor" }, { key: "n", label: "deny" }], secondary: ["1 of 2 pending"] } }) },
    { id: "el-approval-edit", title: "Approval \u2014 edit diff, inline", widths: [76], rows: c("ToolCall", { name: "edit", target: EDGE, targetTone: "ref", state: "awaiting", outcome: "+3 \u22123 \u00b7 1 of 1 files", body: EDIT_DIFF,
      decision: { options: [{ key: "y", label: "allow once" }, { key: "a", label: "session" }, { key: "p", label: "project", disabled: true }, { key: "n", label: "deny" }], secondary: ["^D review"] } }) },
    { id: "el-endings", title: "TurnNotice \u2014 turn endings and errors", widths: [76], rows: evs([
      c("TurnNotice", { kind: "cancelled", title: "cancelled at 12.4s", facts: [["cost", "request 3 \u00b7 in 14.2k \u00b7 out 0.4k \u00b7 \u2014"], ["kept", "journal \u00b7 shell settled as cancelled \u00b7 dropped 1 queued"]] }),
      c("TurnNotice", { title: "rate limited \u00b7 glm/5.3 \u00b7 HTTP 429", facts: [["cost", "request 7 \u00b7 in 22.9k \u00b7 \u2014"], ["kept", "journal \u00b7 3 files changed \u00b7 w2 running"], ["next", "wait for the window, or /model"]] }),
      c("TurnNotice", { title: "authentication failed \u00b7 claude (anthropic-subscription)", facts: [["kept", "journal"], ["next", "sign in to Claude Code again \u00b7 p1 login --list"]] }),
      c("TurnNotice", { title: "account exhausted \u00b7 deepseek2 (opencode-go-2-subscription) \u00b7 not retried", facts: [["kept", "journal \u00b7 1 file changed"], ["next", "/model to continue on another route"]] }),
      c("TurnNotice", { title: "connection failed \u00b7 Transport: chat stream ended before [DONE]", facts: [["cost", "request 14 \u00b7 1.2k out streamed, not kept"], ["kept", "journal"], ["next", "send again to continue \u00b7 /model"]] }),
      c("TurnNotice", { title: "journal commit failed \u00b7 No space left on device (os error 28)", facts: [["kept", "nothing after the last committed record happened"], ["next", "free space, then restart p1 to resume"]] }),
      c("TurnNotice", { title: "context failed \u00b7 at the wall: 118k of 120k", facts: [["cost", "summary request \u00b7 in 96.4k \u00b7 \u2014"], ["kept", "history unchanged \u00b7 journal"], ["next", "/model to a larger window"]] }),
      c("TurnNotice", { kind: "stopped", title: "stopped \u00b7 max output tokens", facts: [["cost", "in 12.1k \u00b7 out 32k \u00b7 \u2014"]] }),
      c("TurnNotice", { title: "switch refused \u00b7 gpt/gpt-5.6-sol", facts: [["reason", "the history holds a call this route cannot carry"], ["kept", "still on claude/opus-5.5:high"]] })]) },
    { id: "el-queue-scroll", title: "QueuedRow and ScrollMark", widths: [76], rows: evs([
      cat([c("QueuedRow", { kind: "steering", text: "use a VecDeque for the pending queue" }), c("QueuedRow", { kind: "follow-up", text: "then run clippy on p1-tui" })]),
      c("ScrollMark", { below: 14, running: "shell running", at: 212, total: 480 })]) },
    { id: "el-ledger", title: "LedgerPane at 38 (grid 30); context at the summarize threshold", widths: [38], rows: evs([
      c("LedgerPane", LEDGER),
      c("LedgerPane", { context: { used: "97.1k", window: "120k", pct: 81, warn: true, summarizeAt: "96k", parts: [["system", null, "1.2k"], ["files", 4, "61.8k"], ["tools", 11, "3.1k"], ["recent", null, "31.0k"]] } })]) },
    { id: "el-output", title: "OutputPane at 56 (grid 48)", widths: [56], rows: c("OutputPane", { handle: "h-0275b8a9", source: [["dim", "shell \u00b7 cargo test -p p1-context \u00b7 "], ["fail", "\u2717"], ["dim", " exit 101"]], range: "80\u201394 of 94",
      lines: [[80, "test compaction::case_6 ... ok"], [81, "test compaction::case_7 ... ok"], [82, ""], [83, "failures:"], [84, ""], [85, "---- compaction::hard_pressure_waits stdout ----"], [86, "thread 'compaction::hard_pressure_waits' panicked at crates/p1-context/src/edge.rs:414:9:"], [87, "assertion `left == right` failed"], [88, "  left: Hard"], [89, " right: Ready"]] }) },
    { id: "el-workers-pane", title: "WorkersPane \u2014 wide (56) with every state; compact (38)", widths: [56], rows: c("WorkersPane", { header: { live: 1, queued: 1, pool: "3/4" }, workers: WORKERS }) },
    { id: "el-workers-pane-38", title: "WorkersPane compact at 38", widths: [38], rows: c("WorkersPane", { header: { live: 1, pool: "3/4" }, workers: WORKERS.slice(0, 4) }) },
    { id: "el-statusline", title: "StatusBar \u2014 116 (120 cols), 96 (100 cols), 76 (80 cols), 52", widths: [116, 96, 76, 52], rows: c("StatusBar", { model: ME, effort: "high", repo: "phaseone", branch: "main", workers: 2, ctx: "10%", spend: "\u2014", clock: "0h14" }) }
  ];

  function EL(id) { return function (W, ctx) { return elements.filter(function (e) { return e.id === id; })[0].rows(W, ctx); }; }
  // ---------- full screens ----------
  function base(extra) {
    return Object.assign({ transcript: evs(SESSION), composer: comp("working"), status: status(), pane: ledger(), paneStrip: strip("ledger") }, extra || {});
  }
  var HOME = { version: "0.1.0", path: "~/dev/phaseone", branch: "main", state: ["no journal in this directory."],
    items: [["/resume", "reopen a previous session"], ["/model", "claude/opus-5.5:high"], ["/access", "full \u00b7 --ask to confirm"], ["/goal", "set the session objective"], ["/help", "commands and keys"]] };
  var homeLedger = function (W, H) { return P.LedgerPane.rows({ width: W, height: H, session: LEDGER.session, context: { used: "\u2014", window: "120k", pct: null, summarizeAt: "96k" } }); };
  var COMMANDS = [
    { label: "/model", desc: "switch model or effort", right: "claude/opus-5.5:high" }, { label: "/effort", desc: "set effort for this model", right: "high" },
    { label: "/goal", desc: "set the session objective" }, { label: "/focus", desc: "transcript only", right: "off" }, { label: "/status", desc: "session facts" },
    { label: "/resume", desc: "reopen a previous session" }, { label: "/access", desc: "access and sandbox", right: "full" }, { label: "/help", desc: "commands and keys" },
    { label: "/models", desc: "every model p1 can run" }, { label: "/exit", desc: "quit p1" }];
  var MODELS = [
    { header: "CLAUDE", right: "anthropic-subscription", rows: [{ label: "claude/opus-5.5", desc: "low medium high max", right: "current" }, { label: "claude/sonnet-5", desc: "low medium high max", right: "oauth \u00b7 borrowed" }, { label: "claude/opus-5", desc: "low medium high", right: "oauth \u00b7 borrowed" }] },
    { header: "DEEPSEEK", right: "opencode-go-subscription", rows: [{ label: "deepseek/v4.1-flash", desc: "default", right: "api key" }] },
    { header: "DEEPSEEK2", right: "opencode-go-2-subscription", rows: [{ label: "deepseek2/v4.1-flash", desc: "default", right: "api key" }] },
    { header: "GLM", right: "glm-subscription", rows: [{ label: "glm/5.3", desc: "default", disabled: true, reason: "account exhausted" }] },
    { header: "GPT", right: "openai-codex-subscription", rows: [{ label: "gpt/gpt-6-astra", desc: "low medium high", right: "oauth \u00b7 borrowed" }, { label: "gpt/gpt-5.6-sol", desc: "low medium high", focusDesc: "effort \u2190 medium \u2192", right: "oauth \u00b7 borrowed" }, { label: "gpt/gpt-5.6-terra", desc: "low medium high", right: "oauth \u00b7 borrowed" }, { label: "gpt/gpt-5.6-luna", desc: "low medium", right: "oauth \u00b7 borrowed" }] }];
  var modelMenu = c("Menu", { title: "/model", filter: "", count: "11 models \u00b7 5 environments", labelWidth: 22, groups: MODELS, focused: 7, note: "switches at the next turn", keys: "\u2191\u2193 move   \u2190\u2192 effort   \u23ce switch   esc" });
  var workersPane = function (W, H) { return P.WorkersPane.rows({ width: W, header: { live: 1, queued: 1, pool: "3/4" }, workers: WORKERS }); };
  var WSESSION = [F.ask, F.read, c("ToolCall", { name: "worker_start", target: "w2 \u00b7 deepseek2/v4.1-flash", status: "ok", outcome: "started", body: [[["ink", "split provider-http helpers into p1-provider-http (#47)"]], { kv: ["grants", "read edit shell finish"] }] }),
    c("ToolCall", { name: "worker_start", target: "w3 \u00b7 deepseek2/v4.1-flash", status: "ok", outcome: "started", body: [[["ink", "audit sandbox read paths"]], { kv: ["grants", "read grep shell finish"] }] }),
    c("WorkerReport", { id: "w1", route: "gpt/gpt-5.6-luna", state: "unverified", elapsed: "2m10s", grants: ["read", "edit", "finish"], line: "done \u00b7 not verified \u2014 parent verification required" }),
    c("ToolCall", { name: "shell", target: "cargo test -p p1-host --test worker_grants", status: "ok", outcome: "8.2s \u00b7 exit 0 \u00b7 41 lines" }),
    c("ProseFlow", { lines: ["w1's change passes the worker_grants suite. Waiting on w2 and w3."] })];

  var screens = [
    { id: "S01", title: "Session \u2014 120\u00d740 reference: streaming, tools, LEDGER pane", W: 120, H: 40, spec: base() },
    { id: "S02", title: "Session \u2014 80\u00d724: pane collapsed, transcript unchanged at 76, statusline folds effort into the chip", W: 80, H: 24, spec: base({ status: status({ workers: 0 }) }) },
    { id: "S03", title: "Session \u2014 100\u00d730: transcript 56, pane 38", W: 100, H: 30, spec: base() },
    { id: "S04", title: "Session \u2014 160\u00d748: transcript 98, pane wide 56 promoted to WORKERS by a live worker", W: 160, H: 48, spec: base({ transcript: evs(WSESSION), pane: workersPane, paneStrip: strip("workers"), status: status({ workers: 1 }) }) },
    { id: "S05", title: "Short \u2014 120\u00d712: focus mode automatic, composer hidden while empty", W: 120, H: 12, spec: base({ composerEmpty: true }) },
    { id: "S06", title: "Overlay \u2014 80\u00d724 with ^L: the pane over the transcript", W: 80, H: 24, spec: base({ overlay: true }) },
    { id: "H01", title: "Home \u2014 first run, no journal, slab monogram", W: 120, H: 40, spec: { transcript: c("HomePrelude", HOME), home: c("Monogram", {}), composer: comp("idle"), status: status({ ctx: "\u2014", clock: "0h00" }), pane: homeLedger, paneStrip: strip("ledger", ["ledger"]) } },
    { id: "H02", title: "Home \u2014 80\u00d724, sessions exist, model not logged in", W: 80, H: 24, spec: { transcript: c("HomePrelude", Object.assign({}, HOME, { state: ["3 sessions in this directory \u00b7 last today 21:10.", [["fail", "\u2717 "], ["ink", "claude/opus-5.5"], ["dim", "  no Claude Code login found \u00b7 sign in to Claude Code, or /model"]]] })), home: c("Monogram", {}), composer: comp("idle"), status: status({ ctx: "\u2014", clock: "0h00" }) } },
    { id: "H03", title: "/resume \u2014 session list", W: 120, H: 40, spec: { transcript: c("HomePrelude", Object.assign({}, HOME, { state: ["3 sessions in this directory \u00b7 last today 21:10."] })), composer: comp("menu", { text: "/resume" }), status: status({ ctx: "\u2014", clock: "0h00" }), pane: homeLedger, paneStrip: strip("ledger", ["ledger"]),
      docked: c("Menu", { title: "/resume", count: "4 sessions \u00b7 this directory", labelWidth: 18, focused: 0, rows: [{ label: "today 21:10", desc: "fix compaction boundary stall", right: "claude/opus-5.5 \u00b7 214 items" }, { label: "today 17:42", desc: "split provider-http helpers (#47)", right: "deepseek2/v4.1-flash \u00b7 96" }, { label: "yesterday 23:05", desc: "worker grants: add_tools", right: "gpt/gpt-5.6-sol \u00b7 311" }, { label: "2026-09-20 14:02", desc: "websocket fallback notice", disabled: true, reason: "in use \u00b7 pid 41210" }], note: "resumes on its recorded model unless /model is set", keys: "\u2191\u2193 move   \u23ce resume   esc" }) } },
    { id: "H04", title: "Resumed \u2014 history painted, then the resume fact", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.reason, F.prose, F.read, F.grep, F.edit, c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", status: "ok", outcome: "exit 0 \u00b7 94 lines" }), c("MetaRow", { text: "resumed today 21:10 \u00b7 214 items \u00b7 claude/opus-5.5:high" }), c("MetaRow", { text: "1 worker not restored on resume" })]), composer: comp("idle"), paneStrip: strip("ledger", ["ledger"]) }) },
    { id: "C01", title: "Command completion \u2014 `/` in an empty composer", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.read, F.edit, F.confirmed]), composer: comp("menu", { text: "/" }), docked: c("Menu", { rows: COMMANDS, labelWidth: 12, focused: 0, keys: "\u2191\u2193 move   tab complete   \u23ce run   esc" }) }) },
    { id: "C02", title: "/model \u2014 environments \u00d7 profiles, effort on the focused row", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.read, F.edit, F.confirmed]), composer: comp("menu", { text: "/model" }), docked: modelMenu }) },
    { id: "C03", title: "/model \u2014 80\u00d724", W: 80, H: 24, spec: base({ transcript: evs([F.ask, F.read, F.edit, F.confirmed]), composer: comp("menu", { text: "/model" }), docked: modelMenu }) },
    { id: "C04", title: "/help \u2014 command output in the transcript", W: 120, H: 40, spec: base({ composer: comp("idle"), transcript: evs([F.confirmed, c("CommandOutput", { command: "/help", facts: "10 commands \u00b7 14 keys", body: [{ head: "COMMANDS" }].concat([["/model [REF]", "switch model or effort \u00b7 claude/opus-5.5:high"], ["/effort LEVEL", "low medium high max"], ["/goal [TEXT]", "set or clear the session objective \u00b7 ^G edits"], ["/focus [on|off]", "transcript only"], ["/status", "session facts"], ["/resume", "reopen a previous session"], ["/access", "access and sandbox (fixed per process)"], ["/models", "every model p1 can run"], ["/exit", "quit"]].map(function (r) { return { left: [["ink", "  " + u.padEnd(r[0], 18)], ["dim", r[1]]] }; })).concat([{ head: "KEYS" }]).concat([["\u23ce  \u2325\u23ce", "send \u00b7 newline; while working: steer \u00b7 follow-up"], ["^C", "cancel the turn \u00b7 quit when idle"], ["^O  ^R", "open the latest fold \u00b7 toggle reasoning"], ["^Tab ^N  ^W  ^P", "pane mode \u00b7 width \u00b7 pin"], ["^F  ^L", "focus the pane \u00b7 pane overlay under 100 cols"], ["^G  PgUp PgDn  esc", "goal \u00b7 scroll \u00b7 live tail / dismiss"]].map(function (r) { return { left: [["ink", "  " + u.padEnd(r[0], 18)], ["dim", r[1]]] }; })) })]) }) },
    { id: "C05", title: "Goal editor \u2014 ^G prefills the composer", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.read, F.edit, F.confirmed]), composer: comp("idle", { text: "/goal fix compaction boundary stall without changing the summary format", hint: "\u23ce set goal   empty \u23ce clears", secondary: "esc keep" }) }) },
    { id: "T01", title: "Streaming \u2014 reasoning expanded, prose streaming, turn working row", W: 120, H: 40, spec: base({ transcript: evs([F.ask, c("Reasoning", { elapsed: "4.2s", expanded: true, lines: ["The wait only exists for the no-summary case. If a summary is ready the boundary can apply it directly; the hard-pressure branch predates the worker summary path."] }), c("ProseFlow", { lines: ["The hard-pressure wait blocks the turn boundary instead of applying the summary the worker already"] }), c("TurnWorking", { phase: "streaming", elapsed: "6.0s", request: 1 })]) }) },
    { id: "T02", title: "Streaming a tool's arguments \u2014 ToolInputDelta before ToolStarted", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.reason, F.prose, F.read, c("ToolCall", { name: "", target: "*** Begin Patch", state: "preparing", size: "1.4 kB", body: ["+if let Some(summary) = ready {", "+    return self.apply_at_boundary(summary);", "+}"] }), c("TurnWorking", { phase: "preparing", elapsed: "2.4s", request: 3 })]) }) },
    { id: "T03", title: "Notices \u2014 provider notice, context replaced, steering delivered, inbox, worker end, retry (proposal)", W: 120, H: 40, spec: base({ transcript: evs([
      c("OperatorTurn", { text: "switch the websocket test to the sse path" }), c("MetaRow", { text: "transport: WebSocket unavailable (426) \u2014 using HTTP (SSE) for the rest of this session" }),
      c("ToolCall", { name: "read", target: "crates/p1-provider-openai/tests/websocket.rs", targetTone: "ref", status: "ok", outcome: "388 lines \u00b7 13.0 kB" }),
      c("MetaRow", { text: "context summarized \u00b7 214 \u2192 31 items", right: [["dim", "in 18.2k \u00b7 out 1.1k"]] }),
      c("OperatorTurn", { text: "keep the fallback notice text constant", tag: "steering" }), c("MetaRow", { text: "1 inbox message delivered" }),
      c("WorkerReport", { id: "w2", route: "deepseek2/v4.1-flash", state: "done", elapsed: "2m10s", grants: ["read", "edit", "shell", "finish"], line: "done \u00b7 verified \u00b7 cargo test -p p1-provider-http" }),
      c("MetaRow", { text: "Transport: chat stream ended before [DONE] \u00b7 retry 1 of 3" }),
      c("TurnWorking", { phase: "retrying \u00b7 1 of 3 \u00b7 in 24s", request: 8 })]), pane: ledger({ context: { used: "31.0k", window: "120k", pct: 26, summarizeAt: "96k" } }) }) },
    { id: "E01", title: "Cancelled \u2014 ^C during a shell call", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.reason, F.prose, F.read, F.grep, F.edit, c("ToolCall", { name: "shell", target: "cargo test -p p1-context boundary", status: "cancelled" }), c("TurnNotice", { kind: "cancelled", title: "cancelled at 12.4s", facts: [["cost", "request 3 \u00b7 in 14.2k \u00b7 out 0.4k \u00b7 \u2014"], ["kept", "journal \u00b7 edge.rs edited \u00b7 dropped 1 queued"]] })]), composer: comp("idle") }) },
    { id: "E02", title: "Provider failed \u2014 exhausted account at 80\u00d724", W: 80, H: 24, spec: base({ transcript: evs([F.ask, F.read, F.edit, c("TurnNotice", { title: "account exhausted \u00b7 deepseek2 (opencode-go-2-subscription) \u00b7 not retried", facts: [["cost", "request 5 \u00b7 in 22.9k \u00b7 \u2014"], ["kept", "journal \u00b7 1 file changed"], ["next", "/model to continue on another route"]] })]), composer: comp("idle"), status: status({ model: "deepseek2/v4.1-flash", effort: "" }) }) },
    { id: "A01", title: "Approval \u2014 permission inline (120\u00d740)", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.read, F.edit, EL("el-approval-permission")]), composer: comp("approval") }) },
    { id: "A02", title: "Approval \u2014 destructive floor, from a worker (80\u00d724)", W: 80, H: 24, spec: base({ transcript: evs([F.ask, F.edit, EL("el-approval-floor")]), composer: comp("approval") }) },
    { id: "A03", title: "Approval \u2014 edit diff inline", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.reason, F.prose, F.read, EL("el-approval-edit")]), composer: comp("approval") }) },
    { id: "A04", title: "Full review \u2014 apply_patch, file 1 of 3 (^D)", W: 120, H: 40, spec: { status: status(), full: function (W, H) { return P.DiffReview.rows({ width: W, height: H, tool: "apply_patch", path: EDGE, summary: "update file \u00b7 hunk 1 of 1", counts: "+12 \u22123", position: 1, total: 3, rows: EDIT_DIFF.concat(EDIT_DIFF.map(function (d) { return Object.assign({}, d, { line: d.line + 40 }); })),
      decision: { options: [{ key: "y", label: "allow once" }, { key: "a", label: "session" }, { key: "p", label: "project", reason: "not available \u2014 no trust store yet" }, { key: "n", label: "deny" }], secondary: ["all 3 files"] } }); } } },
    { id: "A05", title: "Full review \u2014 80\u00d724, body scrolls, decision pinned", W: 80, H: 24, spec: { status: status(), full: function (W, H) { return P.DiffReview.rows({ width: W, height: H, tool: "edit", path: EDGE, summary: "replace exact string \u00b7 once", counts: "+3 \u22123", position: 1, total: 1, rows: EDIT_DIFF.concat(EDIT_DIFF),
      decision: { options: [{ key: "y", label: "allow once" }, { key: "a", label: "session" }, { key: "p", label: "project", disabled: true }, { key: "n", label: "deny" }] } }); } } },
    { id: "W01", title: "Workers \u2014 160\u00d748, WORKERS pane with every state, one needs review", W: 160, H: 48, spec: base({ transcript: evs(WSESSION), composer: comp("working"), pane: workersPane, paneStrip: c("PaneStrip", { modes: ["ledger", "output", "workers"], current: "workers", pinned: true }), status: status({ workers: 1 }) }) },
    { id: "W02", title: "Attached \u2014 a worker's own transcript (a), read-only composer", W: 120, H: 40, spec: base({ attach: c("AttachBand", { id: "w2", route: "deepseek2/v4.1-flash", state: "running" }), transcript: evs([c("OperatorTurn", { text: "split provider-http helpers into p1-provider-http (#47)" }), c("ToolCall", { name: "read", target: "crates/p1-provider-openai-chat/src/lib.rs", targetTone: "ref", status: "ok", outcome: "612 lines \u00b7 21.4 kB" }), c("ToolCall", { name: "grep", target: "http_error_code crates/", status: "ok", outcome: "4 hits \u00b7 3 files" }), c("ToolCall", { name: "edit", target: "crates/p1-provider-http/src/retry.rs", targetTone: "ref", state: "running", elapsed: "0.3s" })]), composer: comp("attached"), pane: function (W, H) { return P.WorkersPane.rows({ width: W, header: { live: 2, pool: "3/4" }, workers: WORKERS.slice(0, 4), focused: "w2" }); }, paneStrip: strip("workers"), status: status({ model: "deepseek2/v4.1-flash", effort: "", workers: 2 }) }) },
    { id: "W03", title: "Stop a worker \u2014 x asks once", W: 120, H: 40, spec: base({ transcript: evs(WSESSION), composer: comp("approval", { placeholder: "decide above" }), docked: function (W) { return [u.row(u.S.plus, [["attn", "! "], ["dim", u.field("stop")], ["ink", "w2"], ["dim", " \u00b7 deepseek2/v4.1-flash \u00b7 running 0m52s"]], [], W)].concat(u.decisionRows({ options: [{ key: "y", label: "stop w2" }, { key: "n", label: "keep" }], secondary: ["esc keep"] }, W)); }, pane: function (W, H) { return P.WorkersPane.rows({ width: W, header: { live: 2, pool: "3/4" }, workers: WORKERS.slice(0, 4), focused: "w2" }); }, paneStrip: strip("workers"), status: status({ workers: 2 }) }) },
    { id: "P01", title: "OUTPUT \u2014 ^O opened the fold, ^W wide (transcript 58, pane 56)", W: 120, H: 40, spec: base({ paneWidth: 56, transcript: evs([F.ask, F.read, F.grep, F.shellFail, F.confirmed]), composer: comp("idle"), pane: function (W) { return EL("el-output")(W); }, paneStrip: strip("output") }) },
    { id: "P02", title: "Peek \u2014 a failed tool over the LEDGER for 3 s", W: 120, H: 40, spec: base({ transcript: evs([F.ask, F.read, F.grep, F.shellFail]), composer: comp("working"), peek: c("Peek", { title: "shell failed", line: "exit 101 \u00b7 hard_pressure_waits" }) }) },
    { id: "P03", title: "DIFF pane (planned) \u2014 160\u00d748", W: 160, H: 48, spec: base({ transcript: evs([F.ask, F.read, F.edit, F.confirmed]), composer: comp("idle"), pane: c("DiffPane", { summary: "3 files \u00b7 +48 \u221212", files: [[EDGE, 12, 3], ["crates/p1-context/src/lib.rs", 30, 9], ["crates/p1-context/tests/boundary.rs", 6, 0]], focusedFile: 0, rows: EDIT_DIFF, note: "planned \u2014 needs before-images of touched files" }), paneStrip: strip("diff", ["ledger", "output", "diff", "workers"]) }) },
    { id: "Q01", title: "Queue \u2014 steering and a follow-up waiting", W: 120, H: 40, spec: base({ queued: cat([c("QueuedRow", { kind: "steering", text: "use a VecDeque for the pending queue" }), c("QueuedRow", { kind: "follow-up", text: "then run clippy on p1-tui" })]) }) },
    { id: "R01", title: "Scrolled back \u2014 new rows below, live tail running", W: 120, H: 40, spec: base({ scrollTop: 0, transcript: evs([F.ask, F.reason, F.prose, F.read, F.grep, F.edit, F.shellFail, F.confirmed, F.edit, F.shellRun]), scrollMark: c("ScrollMark", { below: 14, running: "shell running", at: 1, total: 47 }) }) },
    { id: "F01", title: "Focus mode \u2014 /focus at 120\u00d740: pane hidden, composer hidden while empty", W: 120, H: 40, spec: base({ focus: true, composerEmpty: true }) }
  ];

  function renderElement(el, W) { return C.strip(el.rows(W, { compact: false }), W); }
  function renderScreen(s) { return C.compose(s.spec, s.W, s.H); }
  g.P1Screens = { elements: elements, screens: screens, renderElement: renderElement, renderScreen: renderScreen, byId: function (id) { return screens.filter(function (s) { return s.id === id; })[0]; } };
})();
