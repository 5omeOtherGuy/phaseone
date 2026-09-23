const W = 76;
function Gap() { return <div style={{ height: "1lh" }} />; }
const EDIT_BODY = [{ diff: "ctx", line: 87, text: "let cfg = load(&path)?;" }, { diff: "del", line: 88, text: "let ctx = Context::new(cfg);" }, { diff: "add", line: 88, text: "let ctx = Context::new(cfg).retries(3);" }, { diff: "ctx", line: 89, text: "run(&ctx).await" }];
function initial() {
  return [
    { t: "in", text: "fix the flaky retry test in src/run.rs" },
    { t: "block", name: "read", arg: "src/run.rs", argTone: "ref", status: "ok", outcome: "212 lines · 6.1 kB", body: [{ diff: "ctx", line: 86, text: "pub async fn run(ctx: &Context) -> Result<()> {" }, { diff: "ctx", line: 87, text: "let cfg = load(&path)?;" }, { diff: "ctx", line: 88, text: "let ctx = Context::new(cfg);" }] },
    { t: "block", name: "search", arg: "\"retries\" src/", status: "ok", outcome: "2 hits", body: ["src/config.rs:41 — pub retries: u8", "src/client.rs:17 — .retries(cfg.retries)"] },
    { t: "prose", lines: ["The retry count is parsed but never passed to Context. One-line fix."] },
    { t: "block", id: "edit", name: "edit", arg: "src/run.rs", argTone: "ref", status: "attn", outcome: "+1 −1 · 1 of 1 files", body: EDIT_BODY,
      decision: { options: [{ key: "y", label: "allow once" }, { key: "a", label: "session" }, { key: "p", label: "project", disabled: true }, { key: "n", label: "deny" }], secondary: ["^D diff"] } },
  ];
}
function Session() {
  const { Block, OperatorInput, Prose, Worker, Band, Composer, Statusline, Pane } = SLAB;
  const [ev, setEv] = React.useState(initial);
  const [val, setVal] = React.useState("");
  const [calls, setCalls] = React.useState(2);
  const [diff, setDiff] = React.useState([0, 0]);
  const [clock, setClock] = React.useState(2);
  const blocked = ev.some(e => e.decision);
  const inputRef = React.useRef(null);
  const scRef = React.useRef(null);
  React.useEffect(() => { const id = setInterval(() => setClock(c => c + 1), 60000); return () => clearInterval(id); }, []);
  React.useEffect(() => { if (scRef.current) scRef.current.scrollTop = scRef.current.scrollHeight; });
  const push = (...xs) => setEv(e => [...e, ...xs]);
  const patch = (id, p) => setEv(e => e.map(x => x.id === id ? { ...x, ...p } : x));
  const runTest = () => {
    push({ t: "block", id: "t1", name: "shell", arg: "cargo test retry", running: true });
    setCalls(c => c + 1);
    setTimeout(() => {
      patch("t1", { running: false, status: "fail", outcome: "11.4s · exit 101", body: ["running 42 tests", "---- run::retry_backoff stdout ----", "assertion `left == right` failed", "   left: 2", "  right: 3"], meta: { left: [["faint", "· 38 more lines folded → [h-2b14]"]], right: [["faint", "^O open in pane"]] } });
      push({ t: "prose", lines: [[["ink", "The backoff loop exits one attempt early at "], ["ref", "src/run.rs:112"], ["ink", "."]], "Delegating the fix and a doc note so they run in parallel."] },
        { t: "block", id: "d1", name: "delegate", arg: "2 workers", outcome: "ceiling 40 calls", workers: true });
    }, 1600);
  };
  const decide = k => {
    if (!blocked) return;
    if (k === "y" || k === "a") { patch("edit", { status: "ok", outcome: "+1 −1 · applied" + (k === "a" ? " · session grant" : ""), decision: null }); setDiff([1, 1]); setCalls(c => c + 1); runTest(); }
    if (k === "n") { patch("edit", { status: "fail", outcome: "denied", decision: null, body: [] }); push({ t: "prose", lines: ["Understood — leaving src/run.rs untouched."] }); }
  };
  React.useEffect(() => {
    const k = e => { if (blocked && e.key.length === 1 && "yan".includes(e.key) && !e.ctrlKey && !e.metaKey) { e.preventDefault(); decide(e.key); } };
    window.addEventListener("keydown", k); return () => window.removeEventListener("keydown", k);
  });
  const submit = v => {
    if (!v.trim() || blocked) return;
    setVal("");
    push({ t: "in", text: v }, { t: "block", id: "s" + Date.now(), name: "shell", arg: "git status -s", status: "ok", outcome: "0.1s", body: [[["fail", " M"], ["dim", " src/run.rs"]]], meta: "exit 0 · 1 line · cwd ~/dev/harness" });
    setCalls(c => c + 1);
  };
  const render = (e, i) => {
    if (e.t === "in") return <OperatorInput key={i} text={e.text} width={W} />;
    if (e.t === "prose") return <Prose key={i} lines={e.lines} width={W} />;
    if (e.workers) return <div key={i}><Block {...e} width={W} />
      <Worker name="s1-backoff" route="deepseek · v4.1-flash" state="running" elapsed="0m41s" cost="$0.01" owns="src/run.rs · tests/run.rs" activity="editing retry_backoff — attempt counter" width={W} />
      <Band bg="var(--h-block)" width={W} />
      <Worker name="s2-notes" route="kimi · k3" state="queued" elapsed="0m00s" cost="—" owns="docs/retry.md" activity="waiting for s1-backoff" width={W} /></div>;
    return <Block key={i} {...e} width={W} />;
  };
  return (
    <div style={{ position: "fixed", inset: 0, background: "var(--h-ground)", fontFamily: "var(--font-mono)", fontSize: 13, lineHeight: 1.5, display: "grid", placeItems: "start center", overflow: "auto" }}>
      <div style={{ width: "120ch", height: "40lh", paddingTop: "1lh", boxSizing: "border-box", display: "grid", gridTemplateRows: "minmax(0, 1fr) auto", rowGap: "0" }}>
        <div style={{ display: "grid", gridTemplateColumns: "2ch 76ch 2ch 38ch 2ch", minHeight: 0, overflow: "hidden" }}>
          <div />
          <div style={{ display: "flex", flexDirection: "column", minHeight: 0 }}>
            <div ref={scRef} style={{ flex: 1, minHeight: 0, overflow: "hidden", display: "flex", flexDirection: "column", justifyContent: "flex-end" }}>
              <div style={{ flexShrink: 0 }}>{ev.map((e, i) => <div key={i}>{i > 0 && <Gap />}{render(e, i)}</div>)}</div>
            </div>
            <Gap />
            <Composer width={W} value={val} onChange={setVal} onSubmit={submit} inputRef={inputRef}
              placeholder={blocked ? "decide above — y allow · a session · n deny" : "message, / for commands"} />
            <Gap />
          </div>
          <div />
          <div style={{ paddingBottom: "1lh", minHeight: 0 }}>
            <Pane width={38} sections={[
              { title: "SESSION", rows: [["route", "Fable 5.1"], ["profile", "careful"], ["access", "workspace-write"]] },
              { title: "LEDGER", rows: [["calls", String(calls)], ["tokens", "38.2k"], ["spend", "—"]] },
              { title: "WORKERS", rows: ev.some(e => e.workers) ? [["▪ s1-backoff", "0m41s", "live"], ["· s2-notes", "queued", "dim", "faint"]] : [["none", "—", "dim"]] },
              { title: "FOLDS", rows: ev.some(e => e.id === "t1" && e.meta) ? [["h-2b14", "shell · 38", "ref"]] : [["none", "—", "dim"]] },
            ]} />
          </div>
          <div />
        </div>
        <div style={{ padding: "0 2ch 1lh" }}>
          <Statusline width={116} repo="harness" branch="main" effort="high" ctx="6%" spend="—" clock={"0h" + String(clock).padStart(2, "0")} added={diff[0]} removed={diff[1]} />
        </div>
      </div>
    </div>
  );
}
window.Session = Session;